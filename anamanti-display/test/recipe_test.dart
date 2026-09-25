// Recipe mode: the RecipeData parser, the controller folding show/dismiss recipe
// events into AssistantState, and the RecipeView 3-tab widget.

import 'dart:async';

import 'package:anamanti_display/src/engine/assistant_controller.dart';
import 'package:anamanti_display/src/engine/recipe_data.dart';
import 'package:anamanti_display/src/rust/api/engine.dart';
import 'package:anamanti_display/src/ui/recipe_view.dart';
import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

const _recipeJson = '''
{"title":"Spaghetti Carbonara","summary":"A Roman classic.",
 "source_url":"https://example.com/carbonara","image_url":"",
 "servings":"4 servings","total_time":"25 minutes",
 "ingredients":["200g spaghetti","2 eggs","100g pancetta"],
 "steps":["Boil the pasta.","Fry the pancetta.","Toss off the heat."]}
''';

WakeWordConfig _cfg() => WakeWordConfig(
  melspecModelPath: '',
  embeddingModelPath: '',
  wakewordModelPath: '',
  modelName: 'test',
  threshold: 0.5,
  activeThreshold: 0.7,
  orchestratorKey: '',
  discoveryTimeoutSecs: BigInt.zero,
  turnTimeoutSecs: BigInt.zero,
  smoothingWindow: 2,
  fireOnPeak: false,
  playbackBufferSecs: 30,
  useAudiorecord: false,
  micSource: 6,
  platformAec: false,
  platformAgc: true,
  platformNs: true,
  cameraProximity: false,
  proximityMotionThreshold: 0,
  proximityReleaseSecs: 0,
);

WakeWordEvent _ev(
  WakeWordEventKind kind, {
  String recipeJson = '',
  String recipeAction = '',
}) => WakeWordEvent(
  kind: kind,
  message: '',
  device: '',
  deviceSampleRate: 0,
  channels: 0,
  rms: 0,
  score: 0,
  model: '',
  transcript: '',
  reply: '',
  timerId: 0,
  timerLabel: '',
  timerRemainingSecs: 0,
  present: false,
  recipeJson: recipeJson,
  weatherJson: '',
  recipeAction: recipeAction,
);

void main() {
  group('RecipeData.tryParse', () {
    test('parses a full recipe payload', () {
      final r = RecipeData.tryParse(_recipeJson)!;
      expect(r.title, 'Spaghetti Carbonara');
      expect(r.summary, 'A Roman classic.');
      expect(r.servings, '4 servings');
      expect(r.totalTime, '25 minutes');
      expect(r.ingredients, hasLength(3));
      expect(r.steps.first, 'Boil the pasta.');
    });

    test('rejects empty, malformed, and title-less payloads', () {
      expect(RecipeData.tryParse(''), isNull);
      expect(RecipeData.tryParse('not json'), isNull);
      expect(RecipeData.tryParse('{"summary":"no title"}'), isNull);
      expect(RecipeData.tryParse('[1,2,3]'), isNull);
    });

    test('tolerates missing list fields', () {
      final r = RecipeData.tryParse('{"title":"Toast"}')!;
      expect(r.title, 'Toast');
      expect(r.ingredients, isEmpty);
      expect(r.steps, isEmpty);
    });
  });

  test('controller opens and closes recipe mode', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
    )..start();
    addTearDown(controller.dispose);

    expect(controller.state.recipeActive, isFalse);

    engine.add(_ev(WakeWordEventKind.showRecipe, recipeJson: _recipeJson));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.recipeActive, isTrue);
    expect(controller.state.recipe!.title, 'Spaghetti Carbonara');

    // A dismiss event clears it.
    engine.add(_ev(WakeWordEventKind.dismissRecipe));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.recipeActive, isFalse);

    // Re-open, then dismiss via the UI method (touch close).
    engine.add(_ev(WakeWordEventKind.showRecipe, recipeJson: _recipeJson));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.recipeActive, isTrue);
    controller.dismissRecipe();
    expect(controller.state.recipeActive, isFalse);
  });

  test(
    'controller drives recipe tabs + scroll and pushes screen context',
    () async {
      final engine = StreamController<WakeWordEvent>.broadcast();
      final ctxCalls = <Map<String, Object?>>[];
      final controller = AssistantController(
        config: _cfg(),
        startEngine: (_) => engine.stream,
        setRecipeContext:
            ({
              required bool active,
              required String title,
              required String tab,
              required bool atTop,
              required bool atBottom,
              required int ingredientCount,
              required int stepCount,
            }) => ctxCalls.add({
              'active': active,
              'tab': tab,
              'atTop': atTop,
              'atBottom': atBottom,
              'ingredientCount': ingredientCount,
              'stepCount': stepCount,
            }),
      )..start();
      addTearDown(controller.dispose);

      // Opening pushes active context on the Overview tab with the right counts.
      engine.add(_ev(WakeWordEventKind.showRecipe, recipeJson: _recipeJson));
      await Future<void>.delayed(Duration.zero);
      expect(controller.state.recipeTab, 0);
      expect(ctxCalls.last['active'], isTrue);
      expect(ctxCalls.last['tab'], 'overview');
      expect(ctxCalls.last['ingredientCount'], 3);
      expect(ctxCalls.last['stepCount'], 3);

      // Voice "show the ingredients" switches the tab and re-pushes context.
      engine.add(
        _ev(WakeWordEventKind.recipeNavigate, recipeAction: 'ingredients'),
      );
      await Future<void>.delayed(Duration.zero);
      expect(controller.state.recipeTab, 1);
      expect(ctxCalls.last['tab'], 'ingredients');

      // Voice "scroll down" bumps the scroll command counter + direction.
      final seqBefore = controller.state.recipeScrollSeq;
      engine.add(_ev(WakeWordEventKind.recipeScroll, recipeAction: 'down'));
      await Future<void>.delayed(Duration.zero);
      expect(controller.state.recipeScrollSeq, seqBefore + 1);
      expect(controller.state.recipeScrollDir, 'down');

      // The view reporting its position refreshes the pushed context.
      controller.reportRecipeScrollPosition(false, true);
      expect(ctxCalls.last['atTop'], isFalse);
      expect(ctxCalls.last['atBottom'], isTrue);

      // A touch of the tab bar routes through the controller too.
      controller.setRecipeTab(2);
      expect(controller.state.recipeTab, 2);
      expect(ctxCalls.last['tab'], 'steps');

      // Closing pushes an inactive context.
      controller.dismissRecipe();
      expect(controller.state.recipeActive, isFalse);
      expect(ctxCalls.last['active'], isFalse);
    },
  );

  test('controller ignores an unparseable recipe payload', () async {
    final engine = StreamController<WakeWordEvent>.broadcast();
    final controller = AssistantController(
      config: _cfg(),
      startEngine: (_) => engine.stream,
    )..start();
    addTearDown(controller.dispose);

    engine.add(_ev(WakeWordEventKind.showRecipe, recipeJson: 'garbage'));
    await Future<void>.delayed(Duration.zero);
    expect(controller.state.recipeActive, isFalse);
  });

  testWidgets('RecipeView renders tabs and switches between them', (
    tester,
  ) async {
    final recipe = RecipeData.tryParse(_recipeJson)!;
    var closed = false;

    await tester.pumpWidget(
      MaterialApp(
        home: RecipeView(recipe: recipe, onClose: () => closed = true),
      ),
    );

    // Title + facts in the header.
    expect(find.text('Spaghetti Carbonara'), findsOneWidget);
    // Overview tab is default: summary is visible.
    expect(find.text('A Roman classic.'), findsOneWidget);

    // Switch to Ingredients.
    await tester.tap(find.byKey(const Key('recipe-tab-1')));
    await tester.pumpAndSettle();
    expect(find.text('200g spaghetti'), findsOneWidget);

    // Switch to Steps.
    await tester.tap(find.byKey(const Key('recipe-tab-2')));
    await tester.pumpAndSettle();
    expect(find.text('Boil the pasta.'), findsOneWidget);

    // Close control invokes onClose.
    await tester.tap(find.byKey(const Key('recipe-close')));
    expect(closed, isTrue);
  });

  testWidgets('RecipeView is tab-controlled and reports scroll position', (
    tester,
  ) async {
    final recipe = RecipeData.tryParse(_recipeJson)!;
    var tab = 0;
    int? tappedTab;
    bool? lastAtTop;

    await tester.pumpWidget(
      MaterialApp(
        home: StatefulBuilder(
          builder: (context, setState) => RecipeView(
            recipe: recipe,
            onClose: () {},
            tab: tab,
            onTabSelected: (i) {
              tappedTab = i;
              setState(() => tab = i);
            },
            onScrollPositionChanged: (t, _) => lastAtTop = t,
          ),
        ),
      ),
    );
    await tester.pumpAndSettle();

    // The view reports its initial pane position after layout.
    expect(lastAtTop, isNotNull);

    // A tab tap routes through onTabSelected; feeding the new `tab` back switches
    // the visible pane.
    await tester.tap(find.byKey(const Key('recipe-tab-2')));
    await tester.pumpAndSettle();
    expect(tappedTab, 2);
    expect(find.text('Boil the pasta.'), findsOneWidget);
  });
}
