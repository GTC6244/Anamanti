// Wake log (Settings → Audio Diagnostics → the log button): a read-only view of the
// newest lines of the on-device wake-word trace, so a missed wake can be looked at
// on the display itself. The full files are fetched with `adb pull` (path shown).

import 'package:flutter/material.dart';

import 'package:anamanti_display/src/rust/api/engine.dart' show wakeTraceTail;

/// How many of the newest lines the viewer shows (about the last five minutes).
const int kWakeLogLines = 400;

/// The newest lines from the running trace.
Future<String> loadWakeLogTail() => wakeTraceTail(maxLines: kWakeLogLines);

class WakeLogView extends StatefulWidget {
  const WakeLogView({super.key, this.load = loadWakeLogTail});

  /// Fetches the lines to show; injectable so tests run with no native library.
  final Future<String> Function() load;

  @override
  State<WakeLogView> createState() => _WakeLogViewState();
}

class _WakeLogViewState extends State<WakeLogView> {
  /// Start at the end: the newest lines are the ones anyone opens this for.
  final ScrollController _scroll = ScrollController();
  late Future<String> _text = widget.load();

  @override
  void dispose() {
    _scroll.dispose();
    super.dispose();
  }

  void _refresh() {
    setState(() {
      _text = widget.load();
    });
  }

  void _jumpToEnd(Duration _) {
    if (_scroll.hasClients) _scroll.jumpTo(_scroll.position.maxScrollExtent);
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(
        title: const Text('Wake log'),
        actions: [
          IconButton(
            key: const Key('wake-log-refresh'),
            icon: const Icon(Icons.refresh),
            tooltip: 'Refresh',
            onPressed: _refresh,
          ),
        ],
      ),
      body: FutureBuilder<String>(
        future: _text,
        builder: (context, snapshot) {
          if (snapshot.connectionState != ConnectionState.done) {
            return const Center(child: CircularProgressIndicator());
          }
          final text = snapshot.data ?? '';
          if (text.isEmpty) {
            return const Center(child: Text('Nothing logged yet'));
          }
          WidgetsBinding.instance.addPostFrameCallback(_jumpToEnd);
          return Scrollbar(
            controller: _scroll,
            child: SingleChildScrollView(
              controller: _scroll,
              padding: const EdgeInsets.all(12),
              child: SelectableText(
                text,
                key: const Key('wake-log-text'),
                style: const TextStyle(fontFamily: 'monospace', fontSize: 12),
              ),
            ),
          );
        },
      ),
      bottomNavigationBar: const Padding(
        padding: EdgeInsets.fromLTRB(12, 4, 12, 12),
        child: Text(
          'Full log: adb pull /sdcard/Android/data/'
          'com.anamanti.anamanti_display/files/wake_trace',
          style: TextStyle(fontSize: 12, color: Colors.white70),
        ),
      ),
    );
  }
}
