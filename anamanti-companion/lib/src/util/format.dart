/// `m:ss` under an hour, `h:mm:ss` above. Negative clamps to zero.
String formatClock(int totalSeconds) {
  final s = totalSeconds < 0 ? 0 : totalSeconds;
  final h = s ~/ 3600;
  final m = (s % 3600) ~/ 60;
  final sec = s % 60;
  String two(int v) => v.toString().padLeft(2, '0');
  return h > 0 ? '$h:${two(m)}:${two(sec)}' : '$m:${two(sec)}';
}

/// A compact human duration: `45s`, `12m`, `3h 5m`, `2d 4h`.
String formatDuration(int totalSeconds) {
  final s = totalSeconds < 0 ? 0 : totalSeconds;
  if (s < 60) return '${s}s';
  if (s < 3600) return '${s ~/ 60}m';
  if (s < 86400) {
    final m = (s % 3600) ~/ 60;
    return m == 0 ? '${s ~/ 3600}h' : '${s ~/ 3600}h ${m}m';
  }
  final h = (s % 86400) ~/ 3600;
  return h == 0 ? '${s ~/ 86400}d' : '${s ~/ 86400}d ${h}h';
}

/// Milliseconds as `850 ms` / `1.4 s`; `—` when unknown.
String formatMs(int? ms) {
  if (ms == null) return '—';
  if (ms < 1000) return '$ms ms';
  return '${(ms / 1000).toStringAsFixed(1)} s';
}

/// A calendar date from Unix seconds, e.g. `2026-10-21`. Local time.
String formatDate(int unixSeconds) {
  if (unixSeconds <= 0) return '';
  final d = DateTime.fromMillisecondsSinceEpoch(unixSeconds * 1000);
  String two(int v) => v.toString().padLeft(2, '0');
  return '${d.year}-${two(d.month)}-${two(d.day)}';
}

/// `fact` → `Fact`.
String titleCase(String s) =>
    s.isEmpty ? s : s[0].toUpperCase() + s.substring(1);
