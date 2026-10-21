// What a covered camera means for the screen (Settings → Display → Night).
//
// The camera-as-lux-sensor (`camera/ambient.rs`) can't tell a covered lens (privacy
// shutter / tape) from a dark room, and a covered camera also can't see anyone, so
// presence reads "nobody here". The user picks how the display behaves then:
//
//  * [CoveredCameraMode.clock] — take the camera at its word: dark + nobody here, so
//    the screen dims to the away clock (a touch or voice turn still wakes it).
//  * [CoveredCameraMode.schedule] — ignore the blind camera and go by the clock: by
//    day the screen stays awake at normal brightness; during the [NightWindow] it
//    dims to the night-level away clock.

/// How the display behaves while the camera lens is covered.
enum CoveredCameraMode {
  /// Dims to the away clock (what the blind camera reports).
  clock,

  /// Awake by day, dimmed during the configured night hours.
  schedule,
}

/// Default night window: 21:00 → 07:00.
const int kDefaultNightStartMinutes = 21 * 60;
const int kDefaultNightEndMinutes = 7 * 60;

/// A daily night window as minutes after local midnight. [startMinutes] may be later
/// than [endMinutes] (the usual case: it wraps past midnight, e.g. 21:00 → 07:00).
/// Equal start and end means an empty window (never night).
class NightWindow {
  const NightWindow({
    this.startMinutes = kDefaultNightStartMinutes,
    this.endMinutes = kDefaultNightEndMinutes,
  });

  final int startMinutes;
  final int endMinutes;

  /// Whether local time [now] falls inside the window.
  bool isNight(DateTime now) {
    final m = now.hour * 60 + now.minute;
    if (startMinutes == endMinutes) return false;
    if (startMinutes < endMinutes) return m >= startMinutes && m < endMinutes;
    return m >= startMinutes || m < endMinutes;
  }

  @override
  bool operator ==(Object other) =>
      other is NightWindow &&
      other.startMinutes == startMinutes &&
      other.endMinutes == endMinutes;

  @override
  int get hashCode => Object.hash(startMinutes, endMinutes);
}

/// "21:00"-style label for minutes after midnight.
String formatMinutesOfDay(int minutes) {
  final h = (minutes ~/ 60) % 24;
  final m = minutes % 60;
  return '${h.toString().padLeft(2, '0')}:${m.toString().padLeft(2, '0')}';
}
