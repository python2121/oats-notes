import Foundation
import FluidAudio

// MARK: - Stateful (streaming) VAD — OatsNotes addition
//
// `fluidaudio_vad_process_samples` starts every call from a fresh Silero LSTM state, so
// feeding it one 256 ms chunk at a time throws the recurrent context away. A stream keeps
// FluidAudio's public `VadStreamState` (LSTM hidden/cell state + the 64-sample context)
// across calls and runs exactly one model inference per 4096-sample chunk through
// `VadManager.processStreamingChunk(_:state:)` (public since FluidAudio 0.14.x). Only the
// probability and the carried model state are used; the stream's speech start/end
// hysteresis events are ignored (callers do their own segmentation). Fed the same chunks in
// order, a stream reproduces `VadManager.process(samples)` exactly.
//
// Thread affinity: like the rest of the bridge, a stream is driven from one thread at a time.

final class BridgeVadStream {
    static let chunkSize = VadManager.chunkSize

    private let manager: VadManager
    private let threshold: Float
    private var state = VadStreamState.initial()

    init(manager: VadManager, threshold: Float) {
        self.manager = manager
        self.threshold = threshold
    }

    /// One chunk of exactly `chunkSize` samples → (probability, isVoiceActive).
    func process(_ chunk: [Float]) throws -> (Float, Bool) {
        let semaphore = DispatchSemaphore(value: 0)
        var output: VadStreamResult?
        var processError: Error?
        let manager = self.manager
        let state = self.state

        Task {
            do {
                output = try await manager.processStreamingChunk(chunk, state: state)
            } catch {
                processError = error
            }
            semaphore.signal()
        }

        semaphore.wait()

        if let error = processError {
            throw error
        }
        guard let result = output else {
            throw BridgeError.noResult
        }
        self.state = result.state
        return (result.probability, result.probability >= threshold)
    }

    func reset() {
        state = VadStreamState.initial()
    }
}

/// Create a VAD stream. Returns NULL if VAD is not initialised (`fluidaudio_initialize_vad`).
/// Free with `fluidaudio_vad_stream_destroy`.
@_cdecl("fluidaudio_vad_stream_create")
public func fluidaudio_vad_stream_create(_ ptr: UnsafeMutableRawPointer?) -> UnsafeMutableRawPointer? {
    guard let ptr = ptr else { return nil }
    let bridge = Unmanaged<FluidAudioBridgeInternal>.fromOpaque(ptr).takeUnretainedValue()
    guard let stream = bridge.makeVadStream() else { return nil }
    return Unmanaged.passRetained(stream).toOpaque()
}

/// Classify exactly one 4096-sample chunk (16 kHz mono Float32), carrying the model state
/// to the next call. Returns 0 on success, -1 on a NULL argument or wrong `count`, -2 if
/// the model failed.
@_cdecl("fluidaudio_vad_stream_process_chunk")
public func fluidaudio_vad_stream_process_chunk(
    _ stream: UnsafeMutableRawPointer?,
    _ samples: UnsafePointer<Float>?,
    _ count: UInt32,
    _ outProbability: UnsafeMutablePointer<Float>?,
    _ outIsVoiceActive: UnsafeMutablePointer<UInt8>?
) -> Int32 {
    guard let stream = stream, let samples = samples,
        Int(count) == BridgeVadStream.chunkSize
    else { return -1 }
    let vad = Unmanaged<BridgeVadStream>.fromOpaque(stream).takeUnretainedValue()
    let chunk = Array(UnsafeBufferPointer(start: samples, count: Int(count)))
    do {
        let (probability, active) = try vad.process(chunk)
        outProbability?.pointee = probability
        outIsVoiceActive?.pointee = active ? 1 : 0
        return 0
    } catch {
        print("VAD stream chunk error: \(error)")
        return -2
    }
}

/// Reset the stream to Silero's initial (zero) state.
@_cdecl("fluidaudio_vad_stream_reset")
public func fluidaudio_vad_stream_reset(_ stream: UnsafeMutableRawPointer?) {
    guard let stream = stream else { return }
    Unmanaged<BridgeVadStream>.fromOpaque(stream).takeUnretainedValue().reset()
}

@_cdecl("fluidaudio_vad_stream_destroy")
public func fluidaudio_vad_stream_destroy(_ stream: UnsafeMutableRawPointer?) {
    guard let stream = stream else { return }
    Unmanaged<BridgeVadStream>.fromOpaque(stream).release()
}
