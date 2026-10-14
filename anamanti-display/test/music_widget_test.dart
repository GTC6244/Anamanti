// Music mode: the MusicData/QueueTrack parsers and the pure-Dart music widgets
// (NowPlayingView, NextUpView, MusicControlOverlay).

import 'package:anamanti_display/src/engine/music_data.dart';
import 'package:anamanti_display/src/ui/music_control_overlay.dart';
import 'package:anamanti_display/src/ui/next_up_view.dart';
import 'package:anamanti_display/src/ui/now_playing_view.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

// A music payload with no artwork (artwork_uri:""), so no widget test ever
// issues a network fetch — including the next_up items.
const _musicJson = '''
{"playing":true,"track_title":"Midnight City","artist":"M83",
 "album":"Hurry Up, We're Dreaming","artwork_uri":"",
 "position_secs":74,"duration_secs":241,"volume_percent":40,
 "next_up":[
   {"track_title":"Wait","artist":"M83","album":"Hurry Up, We're Dreaming","artwork_uri":""},
   {"track_title":"Reunion","artist":"M83","album":"Hurry Up, We're Dreaming","artwork_uri":""}
 ]}
''';

void main() {
  group('MusicData.tryParse', () {
    test('parses a full music payload with a queue', () {
      final m = MusicData.tryParse(_musicJson)!;
      expect(m.playing, isTrue);
      expect(m.trackTitle, 'Midnight City');
      expect(m.artist, 'M83');
      expect(m.album, "Hurry Up, We're Dreaming");
      expect(m.positionSecs, 74);
      expect(m.durationSecs, 241);
      expect(m.volumePercent, 40);
      expect(m.hasArtwork, isFalse);
      expect(m.nextUp, hasLength(2));
      expect(m.nextUp.first.trackTitle, 'Wait');
      expect(m.nextUp.first.artist, 'M83');
      expect(m.nextUp.first.hasArtwork, isFalse);
      expect(m.nextUp[1].trackTitle, 'Reunion');
    });

    test('rejects empty, malformed, non-map, and trackless payloads', () {
      expect(MusicData.tryParse(''), isNull);
      expect(MusicData.tryParse('   '), isNull);
      expect(MusicData.tryParse('not json'), isNull);
      expect(MusicData.tryParse('[1,2,3]'), isNull);
      expect(MusicData.tryParse('{"artist":"x"}'), isNull); // no track_title
      expect(MusicData.tryParse('{"track_title":""}'), isNull); // empty title
    });

    test('tolerates missing optional fields', () {
      final m = MusicData.tryParse('{"track_title":"Solo"}')!;
      expect(m.trackTitle, 'Solo');
      expect(m.playing, isFalse);
      expect(m.artist, '');
      expect(m.album, '');
      expect(m.positionSecs, 0);
      expect(m.durationSecs, 0);
      expect(m.volumePercent, 0);
      expect(m.nextUp, isEmpty);
      expect(m.hasArtwork, isFalse);
    });

    test('skips malformed next_up items', () {
      const json = '''
      {"track_title":"T","next_up":[
        {"track_title":"Good","artwork_uri":""},
        {"artist":"no title"},
        "garbage",
        {"track_title":""}
      ]}
      ''';
      final m = MusicData.tryParse(json)!;
      expect(m.nextUp, hasLength(1));
      expect(m.nextUp.first.trackTitle, 'Good');
    });

    test('tolerates a numeric double for int fields', () {
      final m = MusicData.tryParse(
        '{"track_title":"T","position_secs":12.0,"volume_percent":55.4}',
      )!;
      expect(m.positionSecs, 12);
      expect(m.volumePercent, 55);
    });
  });

  group('formatClock', () {
    test('formats m:ss with zero-padded seconds', () {
      expect(formatClock(0), '0:00');
      expect(formatClock(9), '0:09');
      expect(formatClock(74), '1:14');
      expect(formatClock(241), '4:01');
      expect(formatClock(-5), '0:00');
    });
  });

  group('NowPlayingView', () {
    testWidgets('renders title, artist, and album', (tester) async {
      final music = MusicData.tryParse(_musicJson)!;
      await tester.pumpWidget(
        MaterialApp(
          home: NowPlayingView(
            music: music,
            onClose: () {},
            onPlayPause: () {},
            onNext: () {},
            onPrevious: () {},
            onVolume: (_) {},
          ),
        ),
      );
      expect(find.text('Midnight City'), findsOneWidget);
      expect(find.text('M83'), findsOneWidget);
      expect(find.text("Hurry Up, We're Dreaming"), findsOneWidget);
    });

    testWidgets('play/pause icon reflects playing state', (tester) async {
      final playing = MusicData.tryParse(_musicJson)!;
      await tester.pumpWidget(
        MaterialApp(
          home: NowPlayingView(
            music: playing,
            onClose: () {},
            onPlayPause: () {},
            onNext: () {},
            onPrevious: () {},
            onVolume: (_) {},
          ),
        ),
      );
      expect(find.byIcon(Icons.pause_circle_filled), findsOneWidget);
      expect(find.byIcon(Icons.play_circle_filled), findsNothing);

      final paused = MusicData.tryParse(
        '{"track_title":"Midnight City","playing":false,"artwork_uri":""}',
      )!;
      await tester.pumpWidget(
        MaterialApp(
          home: NowPlayingView(
            music: paused,
            onClose: () {},
            onPlayPause: () {},
            onNext: () {},
            onPrevious: () {},
            onVolume: (_) {},
          ),
        ),
      );
      expect(find.byIcon(Icons.play_circle_filled), findsOneWidget);
      expect(find.byIcon(Icons.pause_circle_filled), findsNothing);
    });

    testWidgets('transport and close keys fire their callbacks', (tester) async {
      final music = MusicData.tryParse(_musicJson)!;
      var closed = false, played = false, next = false, previous = false;
      await tester.pumpWidget(
        MaterialApp(
          home: NowPlayingView(
            music: music,
            onClose: () => closed = true,
            onPlayPause: () => played = true,
            onNext: () => next = true,
            onPrevious: () => previous = true,
            onVolume: (_) {},
          ),
        ),
      );

      await tester.tap(find.byKey(const Key('music-close')));
      await tester.tap(find.byKey(const Key('music-play-pause')));
      await tester.tap(find.byKey(const Key('music-next')));
      await tester.tap(find.byKey(const Key('music-previous')));
      expect(closed, isTrue);
      expect(played, isTrue);
      expect(next, isTrue);
      expect(previous, isTrue);
    });

    testWidgets('dragging the volume slider yields an int', (tester) async {
      final music = MusicData.tryParse(_musicJson)!;
      int? volume;
      await tester.pumpWidget(
        MaterialApp(
          home: NowPlayingView(
            music: music,
            onClose: () {},
            onPlayPause: () {},
            onNext: () {},
            onPrevious: () {},
            onVolume: (v) => volume = v,
          ),
        ),
      );

      await tester.drag(find.byKey(const Key('music-volume')), const Offset(120, 0));
      expect(volume, isNotNull);
      expect(volume, isA<int>());
    });

    testWidgets('show-queue button appears only with a callback and a queue',
        (tester) async {
      final music = MusicData.tryParse(_musicJson)!;
      var shown = false;
      await tester.pumpWidget(
        MaterialApp(
          home: NowPlayingView(
            music: music,
            onClose: () {},
            onPlayPause: () {},
            onNext: () {},
            onPrevious: () {},
            onVolume: (_) {},
            onShowQueue: () => shown = true,
          ),
        ),
      );
      expect(find.byKey(const Key('music-show-queue')), findsOneWidget);
      expect(find.text('Up next (2)'), findsOneWidget);
      await tester.tap(find.byKey(const Key('music-show-queue')));
      expect(shown, isTrue);

      // No callback → no button.
      await tester.pumpWidget(
        MaterialApp(
          home: NowPlayingView(
            music: music,
            onClose: () {},
            onPlayPause: () {},
            onNext: () {},
            onPrevious: () {},
            onVolume: (_) {},
          ),
        ),
      );
      expect(find.byKey(const Key('music-show-queue')), findsNothing);
    });
  });

  group('NextUpView', () {
    testWidgets('renders the queued track titles and closes', (tester) async {
      final music = MusicData.tryParse(_musicJson)!;
      var closed = false;
      await tester.pumpWidget(
        MaterialApp(
          home: NextUpView(music: music, onClose: () => closed = true),
        ),
      );
      expect(find.text('Up Next'), findsOneWidget);
      expect(find.text('Wait'), findsOneWidget);
      expect(find.text('Reunion'), findsOneWidget);

      await tester.tap(find.byKey(const Key('next-up-close')));
      expect(closed, isTrue);
    });

    testWidgets('back button appears and fires only with a callback',
        (tester) async {
      final music = MusicData.tryParse(_musicJson)!;
      var backed = false;
      await tester.pumpWidget(
        MaterialApp(
          home: NextUpView(
            music: music,
            onClose: () {},
            onBack: () => backed = true,
          ),
        ),
      );
      expect(find.byKey(const Key('next-up-back')), findsOneWidget);
      await tester.tap(find.byKey(const Key('next-up-back')));
      expect(backed, isTrue);
    });

    testWidgets('empty queue shows the empty state', (tester) async {
      final music = MusicData.tryParse('{"track_title":"Solo","artwork_uri":""}')!;
      await tester.pumpWidget(
        MaterialApp(home: NextUpView(music: music, onClose: () {})),
      );
      expect(find.text('Nothing queued'), findsOneWidget);
    });
  });

  group('MusicControlOverlay', () {
    testWidgets('renders the track title and fires transport callbacks',
        (tester) async {
      final music = MusicData.tryParse(_musicJson)!;
      var played = false, next = false, previous = false, opened = false;
      int? volume;
      await tester.pumpWidget(
        MaterialApp(
          home: Scaffold(
            body: Center(
              child: MusicControlOverlay(
                music: music,
                onPlayPause: () => played = true,
                onNext: () => next = true,
                onPrevious: () => previous = true,
                onVolume: (v) => volume = v,
                onTap: () => opened = true,
              ),
            ),
          ),
        ),
      );

      expect(find.text('Midnight City'), findsOneWidget);

      await tester.tap(find.byKey(const Key('music-overlay-play-pause')));
      await tester.tap(find.byKey(const Key('music-overlay-next')));
      await tester.tap(find.byKey(const Key('music-overlay-previous')));
      await tester.tap(find.byKey(const Key('music-overlay-open')));
      await tester.tap(find.byKey(const Key('music-overlay-volume')));
      expect(played, isTrue);
      expect(next, isTrue);
      expect(previous, isTrue);
      expect(opened, isTrue);
      expect(volume, 35); // 40 - 5 step
    });
  });
}
