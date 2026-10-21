//! Codec tests: round-trips for every constructor, golden on-the-wire bytes (the
//! guard that the encoding never drifts for either the Core or the device), and the
//! A10 reader bounds.

use super::*;

async fn roundtrip(event: &WyomingEvent) -> WyomingEvent {
    decode(&event.to_bytes())
        .await
        .expect("decode ok")
        .expect("event present")
}

fn assert_invalid_data(res: io::Result<Option<WyomingEvent>>) {
    match res {
        Err(e) => assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}"),
        Ok(ev) => panic!("expected InvalidData, got {ev:?}"),
    }
}

// ---------------------------------------------------------------------------
// Golden bytes: the exact wire encoding both crates produced before the codec was
// shared. `serde_json` (no `preserve_order`) serializes object keys sorted, and the
// header fields in declaration order.
// ---------------------------------------------------------------------------

#[test]
fn golden_audio_chunk_with_payload() {
    let ev = WyomingEvent::audio_chunk(AudioFormat::PCM_16K_MONO, 100, vec![1, 2, 3, 4]);
    let mut want = b"{\"type\":\"audio-chunk\",\"data_length\":53,\"payload_length\":4}\n\
{\"channels\":1,\"rate\":16000,\"timestamp\":100,\"width\":2}"
        .to_vec();
    want.extend_from_slice(&[1, 2, 3, 4]);
    assert_eq!(ev.to_bytes(), want);
}

#[test]
fn golden_audio_start_and_stop() {
    assert_eq!(
        WyomingEvent::audio_start(AudioFormat::PCM_16K_MONO, 0).to_bytes(),
        b"{\"type\":\"audio-start\",\"data_length\":51}\n\
{\"channels\":1,\"rate\":16000,\"timestamp\":0,\"width\":2}"
    );
    assert_eq!(
        WyomingEvent::audio_stop(7).to_bytes(),
        b"{\"type\":\"audio-stop\",\"data_length\":15}\n{\"timestamp\":7}"
    );
    // A bare event has no length fields at all.
    assert_eq!(
        WyomingEvent::new(types::AUDIO_STOP).to_bytes(),
        b"{\"type\":\"audio-stop\"}\n"
    );
}

#[test]
fn golden_followup_audio_start() {
    let ev = WyomingEvent::audio_start_followup(AudioFormat::PCM_16K_MONO, 0, 2, 10);
    assert_eq!(
        ev.to_bytes(),
        b"{\"type\":\"audio-start\",\"data_length\":101}\n\
{\"channels\":1,\"followup\":true,\"followup_depth\":2,\"rate\":16000,\"timestamp\":0,\"wait_secs\":10,\"width\":2}"
    );
}

#[test]
fn golden_anamanti_frames() {
    assert_eq!(
        WyomingEvent::interrupt().to_bytes(),
        b"{\"type\":\"anamanti-interrupt\"}\n"
    );
    assert_eq!(
        WyomingEvent::timer_start(300, Some("pasta")).to_bytes(),
        b"{\"type\":\"anamanti-timer\",\"data_length\":54}\n\
{\"action\":\"start\",\"duration_secs\":300,\"label\":\"pasta\"}"
    );
    assert_eq!(
        WyomingEvent::timer_cancel(None).to_bytes(),
        b"{\"type\":\"anamanti-timer\",\"data_length\":32}\n{\"action\":\"cancel\",\"label\":null}"
    );
    assert_eq!(
        WyomingEvent::hello("echo-show-8", "", "Kitchen").to_bytes(),
        b"{\"type\":\"anamanti-hello\",\"data_length\":77}\n\
{\"device_id\":\"echo-show-8\",\"instance_id\":\"\",\"name\":\"Kitchen\",\"role\":\"notify\"}"
    );
    assert_eq!(
        WyomingEvent::hello_weather("d", "", "").to_bytes(),
        b"{\"type\":\"anamanti-hello\",\"data_length\":61}\n\
{\"device_id\":\"d\",\"instance_id\":\"\",\"name\":\"\",\"role\":\"weather\"}"
    );
    assert_eq!(
        WyomingEvent::listen(2, 10).to_bytes(),
        b"{\"type\":\"anamanti-listen\",\"data_length\":26}\n{\"depth\":2,\"wait_secs\":10}"
    );
    assert_eq!(
        WyomingEvent::speak("hi").to_bytes(),
        b"{\"type\":\"anamanti-speak\",\"data_length\":13}\n{\"text\":\"hi\"}"
    );
    assert_eq!(
        WyomingEvent::weather_dismiss().to_bytes(),
        b"{\"type\":\"anamanti-weather\",\"data_length\":20}\n{\"action\":\"dismiss\"}"
    );
    assert_eq!(
        WyomingEvent::font_adjust("increase").to_bytes(),
        b"{\"type\":\"anamanti-font\",\"data_length\":42}\n\
{\"action\":\"adjust\",\"direction\":\"increase\"}"
    );
}

#[test]
fn golden_stt_tts_frames() {
    assert_eq!(
        WyomingEvent::synthesize("hi", Some("amy")).to_bytes(),
        b"{\"type\":\"synthesize\",\"data_length\":36}\n{\"text\":\"hi\",\"voice\":{\"name\":\"amy\"}}"
    );
    assert_eq!(
        WyomingEvent::transcript("ok").to_bytes(),
        b"{\"type\":\"transcript\",\"data_length\":13}\n{\"text\":\"ok\"}"
    );
}

#[test]
fn type_constants_are_stable() {
    // The wire names are the contract; a rename would silently break the other side.
    let pairs = [
        (types::AUDIO_START, "audio-start"),
        (types::AUDIO_CHUNK, "audio-chunk"),
        (types::AUDIO_STOP, "audio-stop"),
        (types::TRANSCRIBE, "transcribe"),
        (types::TRANSCRIPT, "transcript"),
        (types::VOICE_STARTED, "voice-started"),
        (types::VOICE_STOPPED, "voice-stopped"),
        (types::SYNTHESIZE, "synthesize"),
        (types::DESCRIBE, "describe"),
        (types::INFO, "info"),
        (types::REPLY_TOKEN, "reply-token"),
        (types::INTERRUPT, "anamanti-interrupt"),
        (types::TIMER, "anamanti-timer"),
        (types::SPEAK, "anamanti-speak"),
        (types::LISTEN, "anamanti-listen"),
        (types::ANAMANTI_HELLO, "anamanti-hello"),
        (types::ANAMANTI_NOTIFY, "anamanti-notify"),
        (types::ANAMANTI_NOTIFY_ACK, "anamanti-notify-ack"),
        (types::RECIPE, "anamanti-recipe"),
        (types::WEATHER, "anamanti-weather"),
        (types::PLACE, "anamanti-place"),
        (types::CAMERA, "anamanti-camera"),
        (types::GET_CAMERA_STREAM, "anamanti-get-camera-stream"),
        (types::CAMERA_STREAM, "anamanti-camera-stream"),
        (types::FONT, "anamanti-font"),
        (types::MUSIC, "anamanti-music"),
        (types::MUSIC_CONTROL, "anamanti-music-control"),
        (types::MUSIC_RESULT, "anamanti-music-result"),
        (types::PAIR_PHONE, "anamanti-pair-phone"),
        (types::PAIR_RESULT, "anamanti-pair-result"),
    ];
    for (got, want) in pairs {
        assert_eq!(got, want);
    }
}

// ---------------------------------------------------------------------------
// Round-trips + decoders
// ---------------------------------------------------------------------------

#[tokio::test]
async fn header_only_event_roundtrips() {
    let ev = WyomingEvent::new(types::AUDIO_STOP);
    let back = roundtrip(&ev).await;
    assert_eq!(back, ev);
    assert_eq!(back.data, Value::Null);
    assert!(back.payload.is_none());
}

#[tokio::test]
async fn audio_start_roundtrips_and_reports_format() {
    let ev = WyomingEvent::audio_start(AudioFormat::PCM_16K_MONO, 42);
    let back = roundtrip(&ev).await;
    assert_eq!(back, ev);
    assert_eq!(audio_format(&back.data), Some(AudioFormat::PCM_16K_MONO));
}

#[tokio::test]
async fn payload_event_roundtrips() {
    let pcm: Vec<u8> = (0..320u16).flat_map(|s| s.to_le_bytes()).collect();
    let ev = WyomingEvent::audio_chunk(AudioFormat::PCM_16K_MONO, 100, pcm.clone());
    let back = roundtrip(&ev).await;
    assert_eq!(back, ev);
    assert_eq!(back.payload.as_deref(), Some(pcm.as_slice()));
}

#[tokio::test]
async fn interrupt_event_roundtrips() {
    let ev = WyomingEvent::interrupt();
    let back = roundtrip(&ev).await;
    assert_eq!(back, ev);
    assert_eq!(back.event_type, types::INTERRUPT);
    assert!(back.is_interrupt());
    assert_eq!(back.data, Value::Null);
    assert!(back.payload.is_none());
}

#[tokio::test]
async fn timer_frame_roundtrips_and_decodes_command() {
    let start = WyomingEvent::timer_start(300, Some("pasta"));
    let back = roundtrip(&start).await;
    assert_eq!(back, start);
    assert_eq!(
        back.timer_command(),
        Some(TimerCommand::Start {
            label: Some("pasta".to_string()),
            duration_secs: 300
        })
    );

    // Cancel-all: null label decodes to `None`.
    let cancel = roundtrip(&WyomingEvent::timer_cancel(None)).await;
    assert_eq!(cancel.data["label"], Value::Null);
    assert_eq!(
        cancel.timer_command(),
        Some(TimerCommand::Cancel { label: None })
    );

    // Adjust: a signed delta (add on a labeled timer) round-trips.
    let add = roundtrip(&WyomingEvent::timer_adjust(120, Some("pasta"))).await;
    assert_eq!(
        add.timer_command(),
        Some(TimerCommand::Adjust {
            label: Some("pasta".to_string()),
            delta_secs: 120
        })
    );
    // Adjust-all with a negative delta (remove time) and a null label.
    let remove = roundtrip(&WyomingEvent::timer_adjust(-60, None)).await;
    assert_eq!(
        remove.timer_command(),
        Some(TimerCommand::Adjust {
            label: None,
            delta_secs: -60
        })
    );

    // A non-timer frame yields no command; a start missing a duration is rejected,
    // as is an adjust missing its delta.
    assert_eq!(WyomingEvent::interrupt().timer_command(), None);
    let bad = WyomingEvent::with_data(types::TIMER, json!({ "action": "start" }));
    assert_eq!(bad.timer_command(), None);
    let bad_adjust = WyomingEvent::with_data(types::TIMER, json!({ "action": "adjust" }));
    assert_eq!(bad_adjust.timer_command(), None);
}

#[tokio::test]
async fn recipe_frame_roundtrips_and_decodes_command() {
    let recipe = json!({
        "title": "Carbonara",
        "ingredients": ["spaghetti", "eggs"],
        "steps": ["boil", "toss"],
    });
    let show = WyomingEvent::recipe(recipe.clone());
    let back = roundtrip(&show).await;
    assert_eq!(back, show);
    assert_eq!(back.recipe_command(), Some(RecipeCommand::Show(recipe)));

    let dismiss = roundtrip(&WyomingEvent::recipe_dismiss()).await;
    assert_eq!(dismiss.recipe_command(), Some(RecipeCommand::Dismiss));

    let nav = roundtrip(&WyomingEvent::recipe_navigate("ingredients")).await;
    assert_eq!(
        nav.recipe_command(),
        Some(RecipeCommand::Navigate("ingredients".to_string()))
    );
    let scroll = roundtrip(&WyomingEvent::recipe_scroll("down")).await;
    assert_eq!(
        scroll.recipe_command(),
        Some(RecipeCommand::Scroll("down".to_string()))
    );
    let step = roundtrip(&WyomingEvent::recipe_step(4)).await;
    assert_eq!(step.recipe_command(), Some(RecipeCommand::Step(4)));

    // A non-recipe frame yields nothing; missing params are rejected.
    assert_eq!(WyomingEvent::interrupt().recipe_command(), None);
    for action in ["show", "navigate", "scroll", "step"] {
        let bad = WyomingEvent::with_data(types::RECIPE, json!({ "action": action }));
        assert_eq!(bad.recipe_command(), None, "{action}");
    }
    // Steps are 1-based: a zero (or negative) step is rejected.
    for n in [json!(0), json!(-2), json!("3")] {
        let bad = WyomingEvent::with_data(types::RECIPE, json!({ "action": "step", "step": n }));
        assert_eq!(bad.recipe_command(), None, "{n}");
    }
}

#[tokio::test]
async fn weather_frame_roundtrips_and_decodes_command() {
    let report = json!({
        "location_label": "Austin, Texas",
        "units": "imperial",
        "current": { "temp": 72, "weather_code": 2, "is_day": true },
        "daily": [{ "date": "2026-09-25", "high": 80, "low": 60 }],
    });
    let show = WyomingEvent::weather_show(report.clone());
    let back = roundtrip(&show).await;
    assert_eq!(back, show);
    assert_eq!(
        back.weather_command(),
        Some(WeatherCommand::Show(report.clone()))
    );

    let current = roundtrip(&WyomingEvent::weather_current(report.clone())).await;
    assert_eq!(
        current.weather_command(),
        Some(WeatherCommand::Current(report))
    );

    let dismiss = roundtrip(&WyomingEvent::weather_dismiss()).await;
    assert_eq!(dismiss.weather_command(), Some(WeatherCommand::Dismiss));

    // A non-weather frame yields nothing; a show missing the report is rejected.
    assert_eq!(WyomingEvent::interrupt().weather_command(), None);
    let bad = WyomingEvent::with_data(types::WEATHER, json!({ "action": "show" }));
    assert_eq!(bad.weather_command(), None);
}

#[tokio::test]
async fn place_frame_roundtrips_and_decodes_command() {
    let report = json!({
        "name": "Blue Bottle Coffee",
        "address": "1 Main St, Austin, TX",
        "hours": ["Monday: 7:00 AM – 6:00 PM"],
        "open_now": true,
        "rating": "4.6",
    });
    let show = WyomingEvent::place_show(report.clone());
    let back = roundtrip(&show).await;
    assert_eq!(back, show);
    assert_eq!(back.place_command(), Some(PlaceCommand::Show(report)));

    let dismiss = roundtrip(&WyomingEvent::place_dismiss()).await;
    assert_eq!(dismiss.place_command(), Some(PlaceCommand::Dismiss));

    assert_eq!(WyomingEvent::interrupt().place_command(), None);
    let bad = WyomingEvent::with_data(types::PLACE, json!({ "action": "show" }));
    assert_eq!(bad.place_command(), None);
}

#[tokio::test]
async fn camera_frames_roundtrip_and_decode() {
    let show = WyomingEvent::camera_show("Front Door");
    let back = roundtrip(&show).await;
    assert_eq!(back, show);
    assert_eq!(
        back.camera_command(),
        Some(CameraCommand::Show("Front Door".to_string()))
    );
    // The show frame never carries a URL/credential.
    assert!(back.data.get("url").is_none());

    let dismiss = roundtrip(&WyomingEvent::camera_dismiss()).await;
    assert_eq!(dismiss.camera_command(), Some(CameraCommand::Dismiss));

    assert_eq!(WyomingEvent::interrupt().camera_command(), None);
    let blank = WyomingEvent::with_data(types::CAMERA, json!({ "action": "show", "camera": " " }));
    assert_eq!(blank.camera_command(), None);
    let unknown = WyomingEvent::with_data(types::CAMERA, json!({ "action": "zoom" }));
    assert_eq!(unknown.camera_command(), None);

    let req = roundtrip(&WyomingEvent::get_camera_stream("Front Door")).await;
    assert_eq!(req.event_type, types::GET_CAMERA_STREAM);
    assert_eq!(req.data["camera"], "Front Door");
}

#[tokio::test]
async fn music_frames_roundtrip_and_decode() {
    let snapshot = json!({ "playing": true, "track_title": "Paranoid Android" });
    let np = roundtrip(&WyomingEvent::music_now_playing(snapshot.clone())).await;
    assert_eq!(np.music_command(), Some(MusicCommand::NowPlaying(snapshot)));
    let dismiss = roundtrip(&WyomingEvent::music_dismiss()).await;
    assert_eq!(dismiss.music_command(), Some(MusicCommand::Dismiss));
    let screen = roundtrip(&WyomingEvent::music_screen("up_next")).await;
    assert_eq!(
        screen.music_command(),
        Some(MusicCommand::Screen("up_next".to_string()))
    );
    // Malformed / foreign frames decode to None.
    let bad = WyomingEvent::with_data(types::MUSIC, json!({ "action": "now_playing" }));
    assert_eq!(bad.music_command(), None);
    assert_eq!(WyomingEvent::interrupt().music_command(), None);

    let ctl = roundtrip(&WyomingEvent::music_control("volume", Some(40))).await;
    assert_eq!(
        ctl.music_control_command(),
        Some(("volume".to_string(), Some(40)))
    );
    let pause = roundtrip(&WyomingEvent::music_control("pause", None)).await;
    assert_eq!(
        pause.music_control_command(),
        Some(("pause".to_string(), None))
    );
    assert_eq!(WyomingEvent::music_dismiss().music_control_command(), None);

    // The display stamps its id so the Core knows which display pressed the button.
    let tagged = roundtrip(
        &WyomingEvent::music_control("display_fullscreen", None).with_device_id(" kitchen "),
    )
    .await;
    assert_eq!(tagged.music_control_device_id(), Some("kitchen"));
    assert_eq!(
        tagged.music_control_command(),
        Some(("display_fullscreen".to_string(), None))
    );
    assert_eq!(pause.music_control_device_id(), None);
    let blank = WyomingEvent::music_control("pause", None).with_device_id("  ");
    assert!(blank.data.get("device_id").is_none());
    let start = WyomingEvent::audio_start(AudioFormat::PCM_16K_MONO, 0).with_device_id("den");
    assert_eq!(start.data["device_id"], serde_json::json!("den"));
    assert_eq!(start.music_control_device_id(), None);

    let hello = WyomingEvent::hello_music("dev", "core", "Kitchen");
    assert_eq!(hello.data["role"], json!("music"));
    assert_eq!(hello.hello_device_id(), Some("dev"));
}

#[tokio::test]
async fn font_adjust_roundtrips_and_decodes_direction() {
    let inc = roundtrip(&WyomingEvent::font_adjust("increase")).await;
    assert_eq!(
        inc.font_command(),
        Some(FontCommand::Adjust("increase".to_string()))
    );
    let dec = roundtrip(&WyomingEvent::font_adjust("decrease")).await;
    assert_eq!(
        dec.font_command(),
        Some(FontCommand::Adjust("decrease".to_string()))
    );

    assert_eq!(WyomingEvent::interrupt().font_command(), None);
    let bad = WyomingEvent::with_data(types::FONT, json!({ "action": "adjust" }));
    assert_eq!(bad.font_command(), None);
}

#[tokio::test]
async fn listen_frame_roundtrips_and_decodes_params() {
    let ev = WyomingEvent::listen(2, 10);
    let back = roundtrip(&ev).await;
    assert_eq!(back, ev);
    assert_eq!(back.event_type, types::LISTEN);
    assert_eq!(back.listen_params(), Some((2, 10)));
    // A non-listen frame yields nothing; a listen missing fields defaults (1, 0).
    assert_eq!(WyomingEvent::interrupt().listen_params(), None);
    let no_field = WyomingEvent::with_data(types::LISTEN, json!({}));
    assert_eq!(no_field.listen_params(), Some((1, 0)));
}

#[tokio::test]
async fn followup_audio_start_marks_depth_and_is_read_back() {
    // depth 0 → a plain audio-start with no follow-up marker.
    let plain = WyomingEvent::audio_start_followup(AudioFormat::PCM_16K_MONO, 0, 0, 0);
    assert_eq!(
        plain,
        WyomingEvent::audio_start(AudioFormat::PCM_16K_MONO, 0)
    );
    assert_eq!(followup_depth(&plain.data), 0);
    assert_eq!(followup_wait_secs(&plain.data), 0);

    // depth > 0 → the PCM header plus followup: true + followup_depth + wait_secs, and
    // the Core-side readers recover exactly what the device stamped.
    let marked = WyomingEvent::audio_start_followup(AudioFormat::PCM_16K_MONO, 0, 3, 5);
    let back = roundtrip(&marked).await;
    assert_eq!(back, marked);
    assert_eq!(audio_format(&back.data), Some(AudioFormat::PCM_16K_MONO));
    assert_eq!(followup_depth(&back.data), 3);
    assert_eq!(followup_wait_secs(&back.data), 5);

    // `followup: false` (or absent) ignores any stray depth.
    let disabled = json!({ "followup": false, "followup_depth": 3 });
    assert_eq!(followup_depth(&disabled), 0);
}

#[tokio::test]
async fn speak_frame_roundtrips_and_extracts_text() {
    let ev = WyomingEvent::speak("Time's up for pasta");
    let back = roundtrip(&ev).await;
    assert_eq!(back, ev);
    assert_eq!(back.event_type, types::SPEAK);
    assert_eq!(back.speak_text(), Some("Time's up for pasta"));
    assert_eq!(WyomingEvent::audio_stop(0).speak_text(), None);
}

#[tokio::test]
async fn notify_frames_roundtrip() {
    let hello = WyomingEvent::hello("echo-show-8", "Paul Family", "Kitchen");
    let back = roundtrip(&hello).await;
    assert_eq!(back, hello);
    assert_eq!(back.event_type, types::ANAMANTI_HELLO);
    assert_eq!(back.hello_device_id(), Some("echo-show-8"));
    assert_eq!(back.hello_name(), Some("Kitchen"));
    assert_eq!(back.hello_role(), "notify");
    assert_eq!(WyomingEvent::audio_stop(0).hello_device_id(), None);
    assert_eq!(WyomingEvent::audio_stop(0).hello_name(), None);

    let weather = roundtrip(&WyomingEvent::hello_weather("dev-1", "core-1", "Bedroom")).await;
    assert_eq!(weather.hello_role(), "weather");
    // A hello without a role (older device) defaults to notify.
    let legacy = WyomingEvent::with_data(types::ANAMANTI_HELLO, json!({ "device_id": "x" }));
    assert_eq!(legacy.hello_role(), "notify");

    let note = WyomingEvent::notify("42-0", "info", "Reminder", "Meeting in 5 minutes");
    let back = roundtrip(&note).await;
    assert_eq!(back, note);
    assert_eq!(back.event_type, types::ANAMANTI_NOTIFY);
    assert_eq!(back.data["title"], json!("Reminder"));
    assert_eq!(back.data["body"], json!("Meeting in 5 minutes"));

    let ack = WyomingEvent::notify_ack("42-0", "shown");
    let back = roundtrip(&ack).await;
    assert_eq!(back, ack);
    assert_eq!(back.event_type, types::ANAMANTI_NOTIFY_ACK);
}

#[tokio::test]
async fn synthesize_carries_text_and_optional_voice() {
    let with_voice = WyomingEvent::synthesize("hello there", Some("en_US-amy-medium"));
    let back = roundtrip(&with_voice).await;
    assert_eq!(back.event_type, types::SYNTHESIZE);
    assert_eq!(back.data["text"], json!("hello there"));
    assert_eq!(back.data["voice"]["name"], json!("en_US-amy-medium"));

    let no_voice = WyomingEvent::synthesize("hi", None);
    assert!(no_voice.data.get("voice").is_none());
}

#[tokio::test]
async fn reply_token_roundtrips() {
    let back = roundtrip(&WyomingEvent::reply_token("Hi ")).await;
    assert_eq!(back.reply_token_text(), Some("Hi "));
    assert_eq!(WyomingEvent::interrupt().reply_token_text(), None);
}

#[test]
fn header_line_is_single_newline_terminated_json() {
    let bytes = WyomingEvent::audio_start(AudioFormat::PCM_16K_MONO, 0).to_bytes();
    let newline = bytes.iter().position(|&b| b == b'\n').expect("has newline");
    // Exactly one newline: the header terminator. The JSON `data` block that follows
    // must not contain a raw newline that could be mistaken for a frame boundary.
    assert_eq!(bytes.iter().filter(|&&b| b == b'\n').count(), 1);
    let header: Header = serde_json::from_slice(&bytes[..newline]).expect("header parses");
    assert_eq!(header.event_type, types::AUDIO_START);
    assert_eq!(header.data_length, Some(bytes.len() - newline - 1));
    assert_eq!(header.payload_length, None);
}

#[test]
fn transcript_text_is_extracted() {
    let ev = WyomingEvent::transcript("turn on the lights");
    assert!(ev.is_transcript());
    assert_eq!(ev.transcript_text(), Some("turn on the lights"));
    let other = WyomingEvent::audio_stop(0);
    assert!(!other.is_transcript());
    assert_eq!(other.transcript_text(), None);
}

#[tokio::test]
async fn empty_input_decodes_to_none() {
    assert_eq!(decode(&[]).await.expect("ok"), None);
}

#[tokio::test]
async fn non_object_data_block_is_rejected() {
    let wire = b"{\"type\":\"x\",\"data_length\":2}\n[]";
    assert_invalid_data(decode(wire).await);
}

#[tokio::test]
async fn two_events_stream_back_to_back() {
    // The length prefixes let the reader split a concatenated stream (payload bytes
    // never confuse the header/frame boundary).
    let mut wire = Vec::new();
    wire.extend_from_slice(&WyomingEvent::audio_start(AudioFormat::PCM_16K_MONO, 0).to_bytes());
    let pcm: Vec<u8> = vec![0x11, 0x22, 0x0a, 0x33]; // includes a 0x0a ('\n') byte
    wire.extend_from_slice(
        &WyomingEvent::audio_chunk(AudioFormat::PCM_16K_MONO, 1, pcm.clone()).to_bytes(),
    );

    let mut reader = &wire[..];
    let first = read_event(&mut reader).await.unwrap().unwrap();
    assert_eq!(first.event_type, types::AUDIO_START);
    let second = read_event(&mut reader).await.unwrap().unwrap();
    assert_eq!(second.event_type, types::AUDIO_CHUNK);
    assert_eq!(second.payload.as_deref(), Some(pcm.as_slice()));
    assert_eq!(read_event(&mut reader).await.unwrap(), None);
}

// ---------------------------------------------------------------------------
// A10: reader bounds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn oversize_header_line_is_rejected() {
    // An unterminated line longer than the cap (a peer streaming garbage).
    let wire = vec![b'a'; MAX_HEADER_LINE_BYTES + 10];
    assert_invalid_data(decode(&wire).await);

    // A newline that only arrives after the cap is still rejected.
    let mut wire = b"{\"type\":\"x\"".to_vec();
    wire.extend(std::iter::repeat_n(b' ', MAX_HEADER_LINE_BYTES));
    wire.extend_from_slice(b"}\n");
    assert_invalid_data(decode(&wire).await);
}

#[tokio::test]
async fn header_line_exactly_at_the_cap_is_accepted() {
    // `{"type":"x"` + padding + `}\n` totalling exactly MAX_HEADER_LINE_BYTES.
    let head = b"{\"type\":\"x\"";
    let mut wire = head.to_vec();
    wire.extend(std::iter::repeat_n(
        b' ',
        MAX_HEADER_LINE_BYTES - head.len() - 2,
    ));
    wire.extend_from_slice(b"}\n");
    assert_eq!(wire.len(), MAX_HEADER_LINE_BYTES);
    let ev = decode(&wire).await.expect("ok").expect("event");
    assert_eq!(ev.event_type, "x");
}

#[tokio::test]
async fn oversize_data_length_is_rejected_without_reading() {
    // No data bytes follow: rejection must come from the declared length alone.
    let wire = format!(
        "{{\"type\":\"x\",\"data_length\":{}}}\n",
        MAX_DATA_BYTES + 1
    );
    assert_invalid_data(decode(wire.as_bytes()).await);

    // A length that could never be allocated fails cleanly instead of aborting.
    let wire = format!("{{\"type\":\"x\",\"data_length\":{}}}\n", usize::MAX);
    assert_invalid_data(decode(wire.as_bytes()).await);
}

#[tokio::test]
async fn oversize_payload_length_is_rejected_without_reading() {
    let wire = format!(
        "{{\"type\":\"audio-chunk\",\"payload_length\":{}}}\n",
        MAX_PAYLOAD_BYTES + 1
    );
    assert_invalid_data(decode(wire.as_bytes()).await);

    // Rejected up front even when a (valid) data block precedes it.
    let wire = format!(
        "{{\"type\":\"audio-chunk\",\"data_length\":2,\"payload_length\":{}}}\n{{}}",
        usize::MAX
    );
    assert_invalid_data(decode(wire.as_bytes()).await);
}

#[tokio::test]
async fn payload_at_the_cap_is_accepted() {
    let ev = WyomingEvent {
        event_type: types::SPEAKER_CLIP.to_string(),
        data: json!({ "ok": true }),
        payload: Some(vec![7u8; MAX_PAYLOAD_BYTES]),
    };
    let back = roundtrip(&ev).await;
    assert_eq!(back.payload.map(|p| p.len()), Some(MAX_PAYLOAD_BYTES));
}
