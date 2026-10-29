import Foundation

// MARK: - Model-path diarization C FFI
//
// Diarize from a pre-staged Sortformer .mlpackage (no HuggingFace download), for
// callers that pin/verify the model themselves. Output marshalling mirrors
// fluidaudio_diarize_file; the caller frees the arrays with
// fluidaudio_free_diarization_result.

@_cdecl("fluidaudio_diarize_file_with_models")
public func fluidaudio_diarize_file_with_models(
    _ ptr: UnsafeMutableRawPointer?,
    _ audioPath: UnsafePointer<CChar>?,
    _ modelPath: UnsafePointer<CChar>?,
    _ outSpeakerIds: UnsafeMutablePointer<UnsafeMutablePointer<UnsafeMutablePointer<CChar>?>?>?,
    _ outStartTimes: UnsafeMutablePointer<UnsafeMutablePointer<Float>?>?,
    _ outEndTimes: UnsafeMutablePointer<UnsafeMutablePointer<Float>?>?,
    _ outQualityScores: UnsafeMutablePointer<UnsafeMutablePointer<Float>?>?,
    _ outCount: UnsafeMutablePointer<UInt32>?
) -> Int32 {
    guard let ptr = ptr, let audioPath = audioPath, let modelPath = modelPath else { return -1 }
    let bridge = Unmanaged<FluidAudioBridgeInternal>.fromOpaque(ptr).takeUnretainedValue()

    let audioString = String(cString: audioPath)
    let modelString = String(cString: modelPath)

    do {
        let segments = try bridge.diarizeFileWithModels(audioPath: audioString, modelPath: modelString)
        emitDiarizationSegments(
            segments,
            outSpeakerIds: outSpeakerIds,
            outStartTimes: outStartTimes,
            outEndTimes: outEndTimes,
            outQualityScores: outQualityScores,
            outCount: outCount
        )
        return 0
    } catch {
        print("Diarize (model path) error: \(error)")
        return -1
    }
}

// MARK: - Warm-up: pre-compile the diarization model
//
// Compile the pre-staged Sortformer `.mlpackage` into the writable per-user cache and
// load it once, paying the one-time ~100s ANE compile up front (populates the e5rt
// cache). Callers warm this at install time so the first real diarize is fast.

@_cdecl("fluidaudio_compile_diarization_model")
public func fluidaudio_compile_diarization_model(
    _ ptr: UnsafeMutableRawPointer?,
    _ modelPath: UnsafePointer<CChar>?
) -> Int32 {
    guard let ptr = ptr, let modelPath = modelPath else { return -1 }
    let bridge = Unmanaged<FluidAudioBridgeInternal>.fromOpaque(ptr).takeUnretainedValue()

    do {
        try bridge.compileDiarizationModel(modelPath: String(cString: modelPath))
        return 0
    } catch {
        print("Compile diarization model error: \(error)")
        return -1
    }
}
