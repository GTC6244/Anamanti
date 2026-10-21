//! Live NVR camera view on the display (`plans/CameraPlan.md`).
//!
//! The Core owns the NVR connection details: host, username, the per-camera channel map,
//! and the **password** (a secret from `REOLINK_PASSWORD`, never in `anamanti.json`).
//! The display never stores any of it. A voice turn ("show the front door") is answered
//! by the zero-model fast path in [`CameraHub::match_utterance`] (or the `show_camera`
//! tool), which sends an `anamanti-camera` **show** frame naming the camera. The display
//! then asks for the stream over the control socket (`anamanti-get-camera-stream`), and
//! [`CameraHub::stream_url`] builds the RTSP sub-stream URL with the credential embedded.
//! That URL rides the device↔Core LAN hop only and is held in memory on the device.
//!
//! Nothing here talks to the NVR: there is no polling and no event subscription (the
//! automatic "someone's at the door" pop-up is deferred, see the plan). The device
//! dials the NVR's RTSP server directly.

use std::sync::Arc;

/// One configured camera: its spoken/display name and its NVR channel **as the NVR's
/// own UI numbers it** (1-based). The RTSP path uses that 1-based number zero-padded
/// to two digits (`h264Preview_05_sub` for channel 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraSpec {
    /// Display name, e.g. `"Front Door"`. Matched case-insensitively by voice.
    pub name: String,
    /// 1-based NVR channel number.
    pub channel: u32,
}

/// The resolved `camera` config block (boot-time only; not runtime-settable yet).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraConfig {
    /// Master switch (`camera.enabled`, default `true` when the block is present).
    pub enabled: bool,
    /// The NVR's host or IP, e.g. `192.168.1.29`. `None` ⇒ the feature is off.
    pub nvr_host: Option<String>,
    /// The NVR's RTSP port (default 554).
    pub rtsp_port: u16,
    /// The NVR user (a dedicated view-only account). `None` ⇒ the feature is off.
    pub username: Option<String>,
    /// The cameras the display can show, in config order (the first is the default).
    pub cameras: Vec<CameraSpec>,
}

impl Default for CameraConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            nvr_host: None,
            rtsp_port: 554,
            username: None,
            cameras: Vec::new(),
        }
    }
}

/// What a transcript asks of the camera view, from the pure-string fast path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CameraIntent {
    /// Open the live view of this camera (its configured display name).
    Show(String),
    /// Close the open camera view.
    Dismiss,
}

/// The configured NVR + cameras + credential. Present (built by [`from_config`]) only
/// when the host, username, password and at least one camera are all set.
#[derive(Clone)]
pub struct CameraHub {
    host: String,
    port: u16,
    username: String,
    password: String,
    cameras: Vec<CameraSpec>,
}

impl std::fmt::Debug for CameraHub {
    // Never print the password.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CameraHub")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("cameras", &self.cameras)
            .finish_non_exhaustive()
    }
}

/// Build the hub from config + the `REOLINK_PASSWORD` secret, or `None` (feature off)
/// when disabled or anything required is missing.
pub fn from_config(cfg: &CameraConfig, password: Option<&str>) -> Option<Arc<CameraHub>> {
    if !cfg.enabled {
        return None;
    }
    let host = cfg
        .nvr_host
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let username = cfg
        .username
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let password = password.filter(|s| !s.is_empty())?;
    let cameras: Vec<CameraSpec> = cfg
        .cameras
        .iter()
        .filter(|c| !c.name.trim().is_empty() && c.channel >= 1)
        .map(|c| CameraSpec {
            name: c.name.trim().to_string(),
            channel: c.channel,
        })
        .collect();
    if cameras.is_empty() {
        return None;
    }
    Some(Arc::new(CameraHub {
        host: host.to_string(),
        port: cfg.rtsp_port,
        username: username.to_string(),
        password: password.to_string(),
        cameras,
    }))
}

impl CameraHub {
    /// The configured cameras, in config order.
    pub fn cameras(&self) -> &[CameraSpec] {
        &self.cameras
    }

    /// Resolve a camera by display name (case/space-insensitive). A blank name means the
    /// first configured camera.
    pub fn resolve(&self, name: &str) -> Option<&CameraSpec> {
        let want = normalize(name);
        if want.is_empty() {
            return self.cameras.first();
        }
        self.cameras.iter().find(|c| normalize(&c.name) == want)
    }

    /// The RTSP **sub-stream** URL for `cam`, with the credential percent-encoded into
    /// the userinfo. The display only ever plays the sub-stream (H.264, small): the
    /// main stream is 4K H.265, too heavy for the Echo Show. Contains the password: never
    /// log it.
    pub fn stream_url(&self, cam: &CameraSpec) -> String {
        format!(
            "rtsp://{}:{}@{}:{}/h264Preview_{:02}_sub",
            encode_userinfo(&self.username),
            encode_userinfo(&self.password),
            self.host,
            self.port,
            cam.channel
        )
    }

    /// The zero-model fast path: is `transcript` a request to show a camera, or (while
    /// one is open, `camera_open`) to close it? Pure string code with no I/O, so a turn
    /// that isn't about the camera costs nothing. `timer_running` keeps a bare
    /// "stop"/"cancel" for the timer, matching the System-1 stop ladder (a running timer
    /// outranks an open screen).
    ///
    /// Deliberately conservative: "open the front door" is **not** a show request (that
    /// could be a door-lock command); "open" only counts with the word "camera".
    pub fn match_utterance(
        &self,
        transcript: &str,
        camera_open: bool,
        timer_running: bool,
    ) -> Option<CameraIntent> {
        let t = normalize(transcript);
        if t.is_empty() {
            return None;
        }
        if camera_open {
            if let Some(intent) = self.match_dismiss(&t, timer_running) {
                return Some(intent);
            }
        }
        self.match_show(&t)
            .map(|c| CameraIntent::Show(c.name.clone()))
    }

    fn match_dismiss(&self, t: &str, timer_running: bool) -> Option<CameraIntent> {
        const BARE: &[&str] = &["stop", "cancel", "stop it"];
        const EXACT: &[&str] = &[
            "close",
            "close it",
            "close that",
            "close this",
            "dismiss",
            "dismiss it",
            "hide",
            "hide it",
            "go back",
            "done",
            "im done",
            "thats enough",
            "that is enough",
            "ok thanks",
            "okay thanks",
            "thank you",
            "thanks",
            "turn it off",
            "never mind",
            "nevermind",
        ];
        let t = strip_politeness(t);
        if BARE.contains(&t) {
            return (!timer_running).then_some(CameraIntent::Dismiss);
        }
        if EXACT.contains(&t) {
            return Some(CameraIntent::Dismiss);
        }
        let verb = ["close", "hide", "dismiss", "stop", "turn off"]
            .iter()
            .any(|v| starts_with_word(t, v));
        (verb && (has_word(t, "camera") || self.named_camera(t).is_some()))
            .then_some(CameraIntent::Dismiss)
    }

    fn match_show(&self, t: &str) -> Option<&CameraSpec> {
        let t = strip_politeness(t);
        // "who's at the door" / "is someone at the front door": a door question.
        let at_door = [
            "whos at",
            "who is at",
            "is someone at",
            "is anyone at",
            "is somebody at",
            "is there someone at",
            "is there anyone at",
        ]
        .iter()
        .any(|p| t.starts_with(p))
            && has_word(t, "door");
        if at_door {
            return self
                .named_camera(t)
                .or_else(|| {
                    self.cameras
                        .iter()
                        .find(|c| has_word(&normalize(&c.name), "door"))
                })
                .or_else(|| self.cameras.first());
        }
        const SHOW_VERBS: &[&str] = &[
            "show me",
            "show",
            "bring up",
            "pull up",
            "put up",
            "display",
            "let me see",
            "can i see",
            "could i see",
            "view",
        ];
        let show_verb = SHOW_VERBS.iter().any(|v| starts_with_word(t, v));
        let mentions_camera = has_word(t, "camera") || has_word(t, "cam") || has_word(t, "cameras");
        // "open"/"turn on" only with the explicit word "camera" (never a bare door).
        let open_verb = ["open", "turn on", "switch to"]
            .iter()
            .any(|v| starts_with_word(t, v));
        if !(show_verb || (open_verb && mentions_camera)) {
            return None;
        }
        if let Some(c) = self.named_camera(t) {
            return Some(c);
        }
        // "show me the camera" with a single camera configured.
        (mentions_camera && self.cameras.len() == 1).then(|| &self.cameras[0])
    }

    /// The camera whose full normalized name appears (as whole words) in `t`; the
    /// longest name wins so "front door left" beats "front door".
    fn named_camera(&self, t: &str) -> Option<&CameraSpec> {
        self.cameras
            .iter()
            .filter(|c| contains_phrase(t, &normalize(&c.name)))
            .max_by_key(|c| c.name.len())
    }
}

/// Lowercase, turn punctuation (except apostrophes, which are dropped so "who's" →
/// "whos") into spaces, and collapse whitespace.
fn normalize(s: &str) -> String {
    let mapped: String = s
        .chars()
        .filter(|c| *c != '\'' && *c != '’')
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect();
    mapped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Drop leading politeness/filler ("hey", "please", "can you", "could you") and a
/// trailing "please", so the verb is first.
fn strip_politeness(mut t: &str) -> &str {
    const LEAD: &[&str] = &[
        "hey ",
        "ok ",
        "okay ",
        "please ",
        "can you ",
        "could you ",
        "would you ",
        "will you ",
    ];
    loop {
        let before = t;
        for p in LEAD {
            if let Some(rest) = t.strip_prefix(p) {
                t = rest;
            }
        }
        if t == before {
            break;
        }
    }
    t.strip_suffix(" please").unwrap_or(t)
}

fn has_word(t: &str, w: &str) -> bool {
    t.split(' ').any(|x| x == w)
}

fn starts_with_word(t: &str, phrase: &str) -> bool {
    t == phrase || t.starts_with(&format!("{phrase} "))
}

fn contains_phrase(t: &str, phrase: &str) -> bool {
    !phrase.is_empty() && format!(" {t} ").contains(&format!(" {phrase} "))
}

/// Percent-encode an RTSP URL userinfo component (RFC 3986 unreserved kept as-is).
fn encode_userinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn hub() -> Arc<CameraHub> {
        from_config(
            &CameraConfig {
                enabled: true,
                nvr_host: Some("192.168.1.29".into()),
                rtsp_port: 554,
                username: Some("anamanti".into()),
                cameras: vec![
                    CameraSpec {
                        name: "Front Door".into(),
                        channel: 5,
                    },
                    CameraSpec {
                        name: "Back yard".into(),
                        channel: 7,
                    },
                ],
            },
            Some("p@ss:w/rd"),
        )
        .expect("configured")
    }

    #[test]
    fn requires_host_user_password_and_a_camera() {
        let mut cfg = CameraConfig {
            nvr_host: Some("nvr".into()),
            username: Some("u".into()),
            cameras: vec![CameraSpec {
                name: "Front Door".into(),
                channel: 5,
            }],
            ..CameraConfig::default()
        };
        assert!(from_config(&cfg, Some("pw")).is_some());
        assert!(from_config(&cfg, None).is_none());
        assert!(from_config(&cfg, Some("")).is_none());
        cfg.enabled = false;
        assert!(from_config(&cfg, Some("pw")).is_none());
        cfg.enabled = true;
        cfg.cameras[0].channel = 0;
        assert!(from_config(&cfg, Some("pw")).is_none());
    }

    #[test]
    fn stream_url_uses_the_one_based_channel_and_encodes_credentials() {
        let h = hub();
        let cam = h.resolve("front door").unwrap();
        assert_eq!(
            h.stream_url(cam),
            "rtsp://anamanti:p%40ss%3Aw%2Frd@192.168.1.29:554/h264Preview_05_sub"
        );
        assert_eq!(h.resolve("").unwrap().name, "Front Door");
        assert!(h.resolve("garage").is_none());
    }

    #[test]
    fn debug_never_prints_the_password() {
        assert!(!format!("{:?}", hub()).contains("p@ss"));
    }

    fn show(s: &str) -> Option<CameraIntent> {
        hub().match_utterance(s, false, false)
    }

    #[test]
    fn show_requests_match() {
        let fd = Some(CameraIntent::Show("Front Door".into()));
        for s in [
            "Show the front door",
            "show me the front door",
            "show me the front door camera.",
            "Can you show the front door, please?",
            "Hey, bring up the front door camera",
            "who's at the door?",
            "Who is at the front door",
            "is someone at the door",
            "let me see the front door",
            "open the front door camera",
        ] {
            assert_eq!(show(s), fd, "{s}");
        }
        assert_eq!(
            show("show the back yard"),
            Some(CameraIntent::Show("Back yard".into()))
        );
    }

    #[test]
    fn non_camera_turns_and_door_commands_fall_through() {
        for s in [
            "open the front door",
            "lock the front door",
            "unlock the front door",
            "is the front door locked",
            "show me the weather",
            "what's the weather",
            "turn off the lights",
            "show the camera", // two cameras: ambiguous, defer to the LLM
            "stop",            // nothing open
            "",
        ] {
            assert_eq!(show(s), None, "{s}");
        }
    }

    #[test]
    fn dismiss_only_while_open_and_timer_keeps_bare_stop() {
        let h = hub();
        for s in [
            "stop",
            "close it",
            "Close the camera",
            "that's enough",
            "hide the front door",
            "thanks",
        ] {
            assert_eq!(
                h.match_utterance(s, true, false),
                Some(CameraIntent::Dismiss),
                "{s}"
            );
        }
        // A running timer outranks the screen for a bare stop…
        assert_eq!(h.match_utterance("stop", true, true), None);
        // …but an explicit camera close still closes it.
        assert_eq!(
            h.match_utterance("close the camera", true, true),
            Some(CameraIntent::Dismiss)
        );
        // Closed view: dismiss phrases don't match.
        assert_eq!(h.match_utterance("close it", false, false), None);
        // While open, a show of another camera still switches.
        assert_eq!(
            h.match_utterance("show the back yard", true, false),
            Some(CameraIntent::Show("Back yard".into()))
        );
    }
}
