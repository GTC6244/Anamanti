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
  captureGainDb: 0,
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
  avgScore: 0,
  threshold: 0,
  gainDb: 0,
  model: '',
  transcript: '',
  reply: '',
  timerId: 0,
  timerLabel: '',
  timerRemainingSecs: 0,
  present: false,
  recipeJson: recipeJson,
  weatherJson: '',
  placeJson: '',
  recipeAction: recipeAction,
  enrollJson: '',
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
              required int highlightedStep,
              required List<String> ingredients,
              required List<String> steps,
            }) => ctxCalls.add({
              'active': active,
              'tab': tab,
              'atTop': atTop,
              'atBottom': atBottom,
              'ingredientCount': ingredientCount,
              'stepCount': stepCount,
              'highlightedStep': highlightedStep,
              'ingredients': ingredients,
              'steps': steps,
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
      // The recipe's lines ride the context (for "how many …?" questions).
      expect(ctxCalls.last['ingredients'], [
        '200g spaghetti',
        '2 eggs',
        '100g pancetta',
      ]);
      expect(ctxCalls.last['steps'], hasLength(3));
      expect(ctxCalls.last['highlightedStep'], 0);

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
      expect(ctxCalls.last['ingredients'], isEmpty);
    },
  );

  test(
    'voice step jump switches to Steps, highlights, and reports it',
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
              required int highlightedStep,
              required List<String> ingredients,
              required List<String> steps,
            }) =>
                ctxCalls.add({'tab': tab, 'highlightedStep': highlightedStep}),
      )..start();
      addTearDown(controller.dispose);

      engine.add(_ev(WakeWordEventKind.showRecipe, recipeJson: _recipeJson));
      await Future<void>.delayed(Duration.zero);

      engine.add(_ev(WakeWordEventKind.recipeStep, recipeAction: '2'));
      await Future<void>.delayed(Duration.zero);
      expect(controller.state.recipeTab, 2);
      expect(controller.state.recipeStep, 2);
      expect(controller.state.recipeStepSeq, 1);
      expect(ctxCalls.last, {'tab': 'steps', 'highlightedStep': 2});

      // Asking for the same step again still bumps the seq (re-scroll).
      engine.add(_ev(WakeWordEventKind.recipeStep, recipeAction: '2'));
      await Future<void>.delayed(Duration.zero);
      expect(controller.state.recipeStepSeq, 2);

      // Out of range / garbage → ignored.
      for (final bad in ['0', '9', 'two', '']) {
        engine.add(_ev(WakeWordEventKind.recipeStep, recipeAction: bad));
        await Future<void>.delayed(Duration.zero);
        expect(controller.state.recipeStep, 2, reason: bad);
      }

      // A tap highlights a step; tapping it again clears the highlight.
      controller.selectRecipeStep(3);
      expect(controller.state.recipeStep, 3);
      expect(ctxCalls.last['highlightedStep'], 3);
      controller.selectRecipeStep(3);
      expect(controller.state.recipeStep, 0);

      // A newly shown recipe starts with no highlight.
      controller.selectRecipeStep(1);
      engine.add(_ev(WakeWordEventKind.showRecipe, recipeJson: _recipeJson));
      await Future<void>.delayed(Duration.zero);
      expect(controller.state.recipeStep, 0);
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

  testWidgets('RecipeView scrolls a far-off step into view and highlights it', (
    tester,
  ) async {
    // A long recipe whose last step starts well below the fold.
    final steps = List.generate(30, (i) => 'Step text number ${i + 1}. ' * 4);
    final recipe = RecipeData(title: 'Long', steps: steps);
    var highlighted = 0;
    var seq = 0;
    var tab = 0;
    late StateSetter rebuild;

    await tester.pumpWidget(
      MaterialApp(
        home: StatefulBuilder(
          builder: (context, setState) {
            rebuild = setState;
            return RecipeView(
              recipe: recipe,
              onClose: () {},
              tab: tab,
              highlightedStep: highlighted,
              stepSeq: seq,
              onStepSelected: (n) => setState(() => highlighted = n),
            );
          },
        ),
      ),
    );
    await tester.pumpAndSettle();

    // Voice "go to step 28": the controller sets the Steps tab + the step + bumps seq.
    rebuild(() {
      tab = 2;
      highlighted = 28;
      seq = 1;
    });
    await tester.pumpAndSettle();

    final bubble = find.byKey(const Key('recipe-step-highlighted'));
    expect(bubble, findsOneWidget);
    expect(tester.widget<Text>(bubble).data, '28');
    // The highlighted step is actually on screen (not just built off-screen).
    final screen = tester.getRect(find.byType(RecipeView));
    final row = tester.getRect(bubble);
    expect(
      screen.contains(row.topLeft) && screen.contains(row.bottomRight),
      isTrue,
    );
    // And step 1 has scrolled out of view.
    final first = tester.getRect(find.text(steps.first));
    expect(first.bottom < screen.top + 80, isTrue);

    // A tap on another step reports it (1-based) and moves the highlight.
    await tester.tap(find.text(steps[26]));
    await tester.pumpAndSettle();
    expect(highlighted, 27);
    expect(tester.widget<Text>(bubble).data, '27');
  });
}
