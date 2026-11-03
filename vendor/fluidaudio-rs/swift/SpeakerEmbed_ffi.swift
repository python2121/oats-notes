import Foundation
import CoreML
import FluidAudio

// MARK: - Speaker embeddings from samples — OatsNotes addition
//
// OatsNotes keeps its own voiceprints (clustering and matching live in Rust), so the bridge
// only answers "which local speakers are in this audio, and what do they sound like": for a
// closed speech segment (≤ ~30 s of 16 kHz mono Float32) it runs FluidAudio's pyannote
// segmentation model over 10 s windows (up to 3 local speakers per window, overlap aware) and
// the WeSpeaker embedding model once per active local speaker, exactly like
// `DiarizerManager.processChunkWithSpeakerTracking` does (clean-frame masks, activity in
// model frames of 0.016875 s, `minActiveFramesCount` 10) — but WITHOUT `SpeakerManager`:
// every window's local speakers come back as separate spans, each with its 256-float
// embedding (L2-normalised here). Models: `DiarizerModels.downloadIfNeeded()` (HF repo
// `FluidInference/speaker-diarization-coreml`: pyannote_segmentation + wespeaker_v2), loaded
// once by `fluidaudio_initialize_speaker_embed`.
//
// Thread affinity: like the rest of the bridge, driven from one thread at a time.

final class BridgeSpeakerEmbedder {
    static let sampleRate = 16_000
    /// The segmentation model's fixed input: 10 s.
    static let windowSamples = 160_000
    /// pyannote segmentation output step (270 samples).
    static let frameStep: Float = 0.016875
    /// `DiarizerConfig.minActiveFramesCount`.
    static let minActiveFrames: Float = 10.0
    /// A trailing window shorter than this (after a full one) is not worth a model call.
    static let minTailSamples = 16_000
    /// Audio shorter than this is taken as ONE speaker: no segmentation, one embedding with
    /// an all-ones mask (FluidAudio's `extractSpeakerEmbedding`), half the model calls. Short
    /// VAD segments are nearly always one voice.
    static let singleSpeakerSamples = 64_000

    struct Span {
        var start: Float
        var end: Float
        var speech: Float
        var embedding: [Float]
    }

    private let models: DiarizerModels
    private let segmentation = SegmentationProcessor()
    private let extractor: EmbeddingExtractor
    private var window = [Float](repeating: 0, count: windowSamples)
    /// Frames of the segmentation output (the embedding model's mask length).
    private let maskFrames: Int

    init(models: DiarizerModels) {
        self.models = models
        self.extractor = EmbeddingExtractor(embeddingModel: models.embeddingModel)
        let shape = models.segmentationModel.modelDescription
            .outputDescriptionsByName["segments"]?.multiArrayConstraint?.shape
        self.maskFrames = (shape?.count ?? 0) >= 2 ? (shape?[1].intValue ?? 589) : 589
    }

    private static func unit(_ e: [Float]) -> [Float]? {
        guard e.allSatisfy({ $0.isFinite }) else { return nil }
        let norm = sqrt(e.reduce(0) { $0 + $1 * $1 })
        guard norm > 0.1 else { return nil }
        return e.map { $0 / norm }
    }

    func spans(_ samples: UnsafeBufferPointer<Float>) throws -> [Span] {
        var out: [Span] = []
        let total = samples.count
        guard total > 0, let base = samples.baseAddress else { return out }
        let windowSamples = Self.windowSamples
        if total < Self.singleSpeakerSamples {
            let audio = Array(samples)
            let mask = [Float](repeating: 1.0, count: maskFrames)
            let embeddings = try extractor.getEmbeddings(audio: audio, masks: [mask])
            guard let first = embeddings.first, let e = Self.unit(first) else { return out }
            let secs = Float(total) / Float(Self.sampleRate)
            out.append(Span(start: 0, end: secs, speech: secs, embedding: e))
            return out
        }
        var offset = 0
        while offset < total {
            let n = min(windowSamples, total - offset)
            if offset > 0 && n < Self.minTailSamples { break }
            window.withUnsafeMutableBufferPointer { buf in
                guard let dst = buf.baseAddress else { return }
                dst.update(from: base.advanced(by: offset), count: n)
                if n < windowSamples {
                    dst.advanced(by: n).update(repeating: 0, count: windowSamples - n)
                }
            }
            let slice = window[0..<windowSamples]
            let (segments, _) = try segmentation.getSegments(
                audioChunk: slice, segmentationModel: models.segmentationModel)
            guard let frames = segments.first, let firstFrame = frames.first else {
                offset += windowSamples
                continue
            }
            let numSpeakers = firstFrame.count
            let numFrames = frames.count
            // frames past the real audio are padding
            let usableFrames = min(
                numFrames,
                Int((Float(n) / Float(Self.sampleRate)) / Self.frameStep) + 1)

            var masks: [[Float]] = Array(
                repeating: [Float](repeating: 0, count: numFrames), count: numSpeakers)
            var activity = [Float](repeating: 0, count: numSpeakers)
            var firstActive = [Int](repeating: -1, count: numSpeakers)
            var lastActive = [Int](repeating: -1, count: numSpeakers)
            for f in 0..<usableFrames {
                let row = frames[f]
                let sum = row.reduce(0, +)
                let clean: Float = sum < 2.0 ? 1.0 : 0.0
                for s in 0..<numSpeakers {
                    let v = row[s]
                    masks[s][f] = v * clean
                    activity[s] += v
                    if v > 0.5 {
                        if firstActive[s] < 0 { firstActive[s] = f }
                        lastActive[s] = f
                    }
                }
            }
            guard activity.contains(where: { $0 > Self.minActiveFrames }) else {
                offset += windowSamples
                continue
            }
            let embeddings = try extractor.getEmbeddings(
                audio: slice, masks: masks, minActivityThreshold: Self.minActiveFrames)
            let offsetS = Float(offset) / Float(Self.sampleRate)
            let maxEnd = Float(n) / Float(Self.sampleRate)
            for s in 0..<numSpeakers where activity[s] > Self.minActiveFrames {
                guard s < embeddings.count, firstActive[s] >= 0 else { continue }
                guard let embedding = Self.unit(embeddings[s]) else { continue }
                let start = Float(firstActive[s]) * Self.frameStep
                let end = min(Float(lastActive[s] + 1) * Self.frameStep, maxEnd)
                out.append(
                    Span(
                        start: offsetS + start,
                        end: offsetS + max(start, end),
                        speech: activity[s] * Self.frameStep,
                        embedding: embedding))
            }
            offset += windowSamples
        }
        return out
    }
}

extension FluidAudioBridgeInternal {
    func initializeSpeakerEmbed() throws {
        if speakerEmbedder != nil { return }
        let semaphore = DispatchSemaphore(value: 0)
        var loaded: DiarizerModels?
        var initError: Error?
        Task {
            do {
                // FluidAudio's default configuration (compute units `.all`): measured faster
                // than `.cpuAndNeuralEngine` and `.cpuOnly` for these two models (2026-11-03)
                loaded = try await DiarizerModels.downloadIfNeeded()
            } catch {
                initError = error
            }
            semaphore.signal()
        }
        semaphore.wait()
        if let error = initError { throw error }
        guard let models = loaded else { throw BridgeError.noResult }
        speakerEmbedder = BridgeSpeakerEmbedder(models: models)
    }
}

/// Download (first run) and load the segmentation + embedding models. 0 on success, -1 on
/// failure (`fluidaudio_last_error` has the reason). Idempotent.
@_cdecl("fluidaudio_initialize_speaker_embed")
public func fluidaudio_initialize_speaker_embed(_ ptr: UnsafeMutableRawPointer?) -> Int32 {
    guard let ptr = ptr else { return -1 }
    let bridge = Unmanaged<FluidAudioBridgeInternal>.fromOpaque(ptr).takeUnretainedValue()
    do {
        try bridge.initializeSpeakerEmbed()
        return 0
    } catch {
        print("Speaker embed init error: \(error)")
        bridge.lastError = "\(error)"
        return -1
    }
}

@_cdecl("fluidaudio_is_speaker_embed_available")
public func fluidaudio_is_speaker_embed_available(_ ptr: UnsafeMutableRawPointer?) -> Int32 {
    guard let ptr = ptr else { return 0 }
    let bridge = Unmanaged<FluidAudioBridgeInternal>.fromOpaque(ptr).takeUnretainedValue()
    return bridge.speakerEmbedder != nil ? 1 : 0
}

/// The local speakers of `count` samples (16 kHz mono Float32): per span its start / end
/// (seconds within the samples), active speech seconds, and `outDim` floats of embedding
/// (span-major in `outEmbeddings`). Arrays are freed with `fluidaudio_free_speaker_spans`;
/// `outCount == 0` yields NULL arrays. 0 on success, -1 on a NULL argument or before init,
/// -2 if a model failed (`fluidaudio_last_error`).
@_cdecl("fluidaudio_speaker_embed_samples")
public func fluidaudio_speaker_embed_samples(
    _ ptr: UnsafeMutableRawPointer?,
    _ samples: UnsafePointer<Float>?,
    _ count: UInt32,
    _ outStarts: UnsafeMutablePointer<UnsafeMutablePointer<Float>?>?,
    _ outEnds: UnsafeMutablePointer<UnsafeMutablePointer<Float>?>?,
    _ outSpeech: UnsafeMutablePointer<UnsafeMutablePointer<Float>?>?,
    _ outEmbeddings: UnsafeMutablePointer<UnsafeMutablePointer<Float>?>?,
    _ outDim: UnsafeMutablePointer<UInt32>?,
    _ outCount: UnsafeMutablePointer<UInt32>?
) -> Int32 {
    outCount?.pointee = 0
    outDim?.pointee = 0
    outStarts?.pointee = nil
    outEnds?.pointee = nil
    outSpeech?.pointee = nil
    outEmbeddings?.pointee = nil
    guard let ptr = ptr, let samples = samples else { return -1 }
    let bridge = Unmanaged<FluidAudioBridgeInternal>.fromOpaque(ptr).takeUnretainedValue()
    guard let embedder = bridge.speakerEmbedder else {
        bridge.lastError = "speaker models not loaded"
        return -1
    }
    let spans: [BridgeSpeakerEmbedder.Span]
    do {
        spans = try embedder.spans(UnsafeBufferPointer(start: samples, count: Int(count)))
    } catch {
        print("Speaker embed error: \(error)")
        bridge.lastError = "\(error)"
        return -2
    }
    let n = spans.count
    if n == 0 { return 0 }
    let dim = spans[0].embedding.count
    let starts = UnsafeMutablePointer<Float>.allocate(capacity: n)
    let ends = UnsafeMutablePointer<Float>.allocate(capacity: n)
    let speech = UnsafeMutablePointer<Float>.allocate(capacity: n)
    let embeddings = UnsafeMutablePointer<Float>.allocate(capacity: n * max(dim, 1))
    for (i, span) in spans.enumerated() {
        starts[i] = span.start
        ends[i] = span.end
        speech[i] = span.speech
        for d in 0..<dim {
            embeddings[i * dim + d] = d < span.embedding.count ? span.embedding[d] : 0
        }
    }
    outStarts?.pointee = starts
    outEnds?.pointee = ends
    outSpeech?.pointee = speech
    outEmbeddings?.pointee = embeddings
    outDim?.pointee = UInt32(dim)
    outCount?.pointee = UInt32(n)
    return 0
}

@_cdecl("fluidaudio_free_speaker_spans")
public func fluidaudio_free_speaker_spans(
    _ starts: UnsafeMutablePointer<Float>?,
    _ ends: UnsafeMutablePointer<Float>?,
    _ speech: UnsafeMutablePointer<Float>?,
    _ embeddings: UnsafeMutablePointer<Float>?
) {
    starts?.deallocate()
    ends?.deallocate()
    speech?.deallocate()
    embeddings?.deallocate()
}
