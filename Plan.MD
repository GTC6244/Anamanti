Project Blueprint: Ambient Smart Display (Echo Show 8 & M4 Mac Mini)1. System Architecture[ Echo Show 8 (LineageOS) ]                                  [ M4 Mac Mini ]
┌─────────────────────────┐                                 ┌────────────────┐
│  Flutter UI Layer       │                                 │                │
│  (Ambient Screen, Text) │                                 │  Wyoming STT   │
└────────────▲────────────┘                                 │  Server        │
             │ (Generated Streams/Callbacks via FRB v2)     │  (Whisper /    │
┌────────────▼────────────┐                                 │  CoreML)       │
│  Rust System Engine     │ ── TCP (Wyoming Protocol) ────► │                │
│  (CPAL/Oboe, OpenWake)  │ ◄── Transcripts (JSON) ──────── │                │
└─────────────────────────┘                                 └────────────────┘
2. Technical StackFrontend: Flutter (configured for always-on landscape layout, handling smooth text renderings and simple image grids).Systems Layer: Rust cross-compiled for Android (aarch64-linux-android).Interop 
Framework: flutter_rust_bridge version 2 (FRB) for seamless zero-copy data flow between Dart and Rust.Audio Capture: cpal crate using the native Android oboe-shared backend.Local 
Wake Word: tract-onnx executing a lightweight .onnx wake word model.Network & Processing 
Backend: Wyoming Protocol running over an asynchronous tokio TCP socket connected to an M4 Mac Mini running a Wyoming-compatible Speech-to-Text (STT) daemon.

3. Implementation Phases
  
 Phase 1: Workspace Setup & Cross-CompilationSet up a unified monorepo workspace containing /lib (Flutter) and /rust directories.Configure flutter_rust_bridge_codegen to watch the code and generate t
 he JNI bindings.Install the Android NDK target via rustup: rustup target add aarch64-linux-android.Validate a basic hello-world cross-compilation pipeline onto the LineageOS target via adb.
 
 Phase 2: Native Audio & Local Inference (Rust Side)Initialize cpal to pull raw 16kHz, mono, 16-bit PCM blocks directly from the Echo Show's microphone array.Pipe the incoming audio buffer streams 
 through a pre-allocated ring buffer.Integrate tract-onnx to read the buffer segments continuously for wake-word confidence levels.Keep this evaluation running completely offline inside a 
 low-overhead Rust thread loop to safeguard the 1GB RAM limitation.
 
 Phase 3: Wyoming Client Core Logic (Rust Side)Implement a lightweight struct that handles the connection state to the Mac Mini via tokio::net::TcpStream.State Transition Rules:Idle Mode: Audio buffer
 is continuously analyzed for the wake word; network socket is closed or dormant.Triggered Mode: The wake word fires. Rust opens the TCP socket, pushes the Wyoming JSON initialization header frame (audio-start), 
 and switches to active transmission.Streaming Mode: Raw audio chunks are sent over TCP wrapped in Wyoming payload frames. Concurrently, listen for incoming transcription events (transcript) on the same socket.
 
 Silence/Timeout Mode: When silence is detected, send an audio-stop frame and safely teardown or reset the streaming state.
 
 Phase 4: Reactive UI & Integration (Flutter Side)Setup the main Dart screen state to process the StreamSink events generated automatically by the Rust framework.Design a modular, 
 responsive layout optimized for an 8-inch smart display screen framework.Connect incoming network messages to state-driven widget updates so words render smoothly across the 
 interface in real-time as the M4 Mac Mini returns tokens.
 
 4. Immediate AI Prompting Targets
When delegating chunks of this project to an AI model for code generation, use these targeted starter contexts:

For the Cargo Setup: "Generate a Cargo.toml file optimized for an Android target (aarch64) utilizing flutter_rust_bridge v2, cpal with oboe, tokio for async infrastructure, and tract-onnx for minimal dependency neural net execution."

For the Protocol Interface: "Write an asynchronous Rust Wyoming protocol implementation using tokio::io. The module must parse string-delimited JSON frames ended by newlines (\n) and handle streaming raw audio chunks immediately after the setup metadata."
