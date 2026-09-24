import 'dart:convert';

/// A parsed recipe pushed from the orchestrator (`recipe_lookup` tool) and rendered
/// on the display's recipe-mode screen (Overview / Ingredients / Steps tabs).
///
/// Mirrors the orchestrator's `crate::recipe::Recipe`: it arrives on the engine
/// stream as the `recipeJson` string of a `WakeWordEventKind.showRecipe` event and
/// is decoded here. All fields tolerate being absent (older/partial payloads).
class RecipeData {
  const RecipeData({
    required this.title,
    this.summary = '',
    this.sourceUrl = '',
    this.imageUrl = '',
    this.servings = '',
    this.totalTime = '',
    this.ingredients = const [],
    this.steps = const [],
  });

  final String title;
  final String summary;
  final String sourceUrl;
  final String imageUrl;
  final String servings;
  final String totalTime;
  final List<String> ingredients;
  final List<String> steps;

  /// Decode from the engine event's `recipeJson`. Returns `null` when the string is
  /// empty, malformed, or has no usable title — so a bad payload never crashes the
  /// UI, it just fails to open recipe mode.
  static RecipeData? tryParse(String jsonStr) {
    if (jsonStr.trim().isEmpty) return null;
    try {
      final decoded = jsonDecode(jsonStr);
      if (decoded is! Map<String, dynamic>) return null;
      final title = (decoded['title'] as String?)?.trim() ?? '';
      if (title.isEmpty) return null;
      return RecipeData(
        title: title,
        summary: (decoded['summary'] as String?) ?? '',
        sourceUrl: (decoded['source_url'] as String?) ?? '',
        imageUrl: (decoded['image_url'] as String?) ?? '',
        servings: (decoded['servings'] as String?) ?? '',
        totalTime: (decoded['total_time'] as String?) ?? '',
        ingredients: _stringList(decoded['ingredients']),
        steps: _stringList(decoded['steps']),
      );
    } catch (_) {
      return null;
    }
  }

  static List<String> _stringList(Object? value) {
    if (value is! List) return const [];
    return value
        .whereType<String>()
        .map((s) => s.trim())
        .where((s) => s.isNotEmpty)
        .toList(growable: false);
  }
}
