import 'package:anamanti_companion/src/data/companion_api.dart';
import 'package:anamanti_companion/src/ui/now_playing_screen.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'support/fakes.dart';
import 'support/host.dart';

void main() {
  testWidgets('shows a Spotify track with progress that advances locally', (
    tester,
  ) async {
    final clock = ManualClock();
    final api = FakeCompanionApi()
      ..nowPlayingResult = NowPlayingStatus(
        linked: true,
        radio: false,
        nowPlaying: sampleTrack(position: 60, duration: 300),
      );
    await tester.pumpWidget(
      host(
        NowPlayingScreen(
          api: api,
          core: sampleCore(),
          active: true,
          clock: clock.call,
        ),
      ),
    );
    await tester.pump();

    expect(find.text('So What'), findsOneWidget);
    expect(find.text('Miles Davis · Kind of Blue'), findsOneWidget);
    expect(find.text('Playing'), findsOneWidget);
    expect(find.text('1:00'), findsOneWidget);
    expect(find.text('5:00'), findsOneWidget);

    clock.advance(const Duration(seconds: 3));
    await tester.pump(const Duration(seconds: 1));
    expect(find.text('1:03'), findsOneWidget);
  });

  testWidgets('polls every 5 s while visible', (tester) async {
    final api = FakeCompanionApi();
    await tester.pumpWidget(
      host(NowPlayingScreen(api: api, core: sampleCore(), active: true)),
    );
    await tester.pump();
    expect(api.nowPlayingCalls, 1);
    await tester.pump(const Duration(seconds: 5));
    expect(api.nowPlayingCalls, 2);
  });

  testWidgets('paused track says Paused', (tester) async {
    final api = FakeCompanionApi()
      ..nowPlayingResult = NowPlayingStatus(
        linked: true,
        radio: false,
        nowPlaying: sampleTrack(playing: false),
      );
    await tester.pumpWidget(
      host(NowPlayingScreen(api: api, core: sampleCore(), active: true)),
    );
    await tester.pump();
    expect(find.text('Paused'), findsOneWidget);
  });

  testWidgets('radio shows the station and no progress bar', (tester) async {
    final api = FakeCompanionApi()
      ..nowPlayingResult = NowPlayingStatus(
        linked: false,
        radio: true,
        nowPlaying: sampleTrack(
          source: 'radio',
          duration: 0,
          stationName: 'CBC Music 2',
        ),
      );
    await tester.pumpWidget(
      host(NowPlayingScreen(api: api, core: sampleCore(), active: true)),
    );
    await tester.pump();
    expect(find.text('CBC Music 2'), findsOneWidget);
    expect(find.byKey(const Key('now-playing-progress')), findsNothing);
  });

  testWidgets('nothing playing vs Spotify not linked', (tester) async {
    final api = FakeCompanionApi();
    await tester.pumpWidget(
      host(NowPlayingScreen(api: api, core: sampleCore(), active: true)),
    );
    await tester.pump();
    expect(find.text('Nothing playing'), findsOneWidget);

    final unlinked = FakeCompanionApi()
      ..nowPlayingResult = const NowPlayingStatus(
        linked: false,
        radio: false,
        nowPlaying: null,
      );
    await tester.pumpWidget(
      host(
        NowPlayingScreen(
          key: const ValueKey('b'),
          api: unlinked,
          core: sampleCore(),
          active: true,
        ),
      ),
    );
    await tester.pump();
    expect(find.text('Spotify not linked'), findsOneWidget);
  });

  testWidgets('no companion_port → clear message', (tester) async {
    final api = FakeCompanionApi();
    await tester.pumpWidget(
      host(
        NowPlayingScreen(
          api: api,
          core: sampleCore(companionPort: null),
          active: true,
        ),
      ),
    );
    expect(
      find.text("This Core doesn't have the companion API turned on"),
      findsOneWidget,
    );
    expect(api.nowPlayingCalls, 0);
  });
}
