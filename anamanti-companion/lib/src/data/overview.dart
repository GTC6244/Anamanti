// A lenient Dart view of the Core's `GET /overview.json` (built by
// `overview_json` in anamanti-core/src/webconfig/system.rs). It crosses the bridge
// as raw JSON so the phone keeps rendering when the Core adds fields; anything
// missing falls back to a neutral default.

import 'dart:convert';

class PipelineStage {
  const PipelineStage({
    required this.key,
    required this.label,
    required this.engine,
    required this.tier,
    required this.status,
    this.detail = '',
    this.keyless = false,
  });

  /// `vad`, `stt`, `system1`, `llm`, `search`, `embed`, `extract`, `tts`.
  final String key;
  final String label;
  final String engine;

  /// `local`, `lan`, `cloud` or `off`.
  final String tier;

  /// `ok`, `error`, `fallback`, `needs_key`, `off` or `unknown` (not probed).
  final String status;
  final String detail;
  final bool keyless;

  factory PipelineStage.fromJson(Map<String, dynamic> j) => PipelineStage(
    key: _str(j['key']),
    label: _str(j['label']),
    engine: _str(j['engine']),
    tier: _str(j['tier'], 'off'),
    status: _str(j['status'], 'unknown'),
    detail: _str(j['detail']),
    keyless: j['keyless'] == true,
  );
}

class ConnectedDisplay {
  const ConnectedDisplay({required this.deviceId, required this.name});
  final String deviceId;
  final String name;
}

class IntegrationStatus {
  const IntegrationStatus({
    required this.key,
    required this.name,
    required this.status,
    this.detail = '',
  });
  final String key;
  final String name;

  /// `active`, `attention`, `inactive` or `disabled`.
  final String status;
  final String detail;
}

class Overview {
  const Overview({
    this.serviceName = '',
    this.instanceId = '',
    this.version = '',
    this.uptimeSecs,
    this.stages = const [],
    this.displays = const [],
    this.integrations = const [],
    this.latencyCount = 0,
    this.p50Ms,
    this.p95Ms,
  });

  final String serviceName;
  final String instanceId;
  final String version;
  final int? uptimeSecs;
  final List<PipelineStage> stages;
  final List<ConnectedDisplay> displays;
  final List<IntegrationStatus> integrations;
  final int latencyCount;
  final int? p50Ms;
  final int? p95Ms;

  List<PipelineStage> get unreachable =>
      stages.where((s) => s.status == 'error').toList();
  List<PipelineStage> get needsAttention => stages
      .where((s) => s.status == 'fallback' || s.status == 'needs_key')
      .toList();

  static Overview parse(String json) {
    final decoded = jsonDecode(json);
    if (decoded is! Map<String, dynamic>) {
      throw const FormatException('overview is not a JSON object');
    }
    return Overview.fromJson(decoded);
  }

  factory Overview.fromJson(Map<String, dynamic> j) {
    final latency = _map(j['latency']);
    return Overview(
      serviceName: _str(j['service_name']),
      instanceId: _str(j['instance_id']),
      version: _str(j['version']),
      uptimeSecs: _int(j['uptime_secs']),
      stages: _list(j['stages']).map(PipelineStage.fromJson).toList(),
      displays: _list(j['devices'])
          .map(
            (d) => ConnectedDisplay(
              deviceId: _str(d['device_id']),
              name: _str(d['name']),
            ),
          )
          .toList(),
      integrations: _list(j['integrations'])
          .map(
            (i) => IntegrationStatus(
              key: _str(i['key']),
              name: _str(i['name']),
              status: _str(i['status'], 'inactive'),
              detail: _str(i['detail']),
            ),
          )
          .toList(),
      latencyCount: _int(latency['count']) ?? 0,
      p50Ms: _int(latency['p50_ms']),
      p95Ms: _int(latency['p95_ms']),
    );
  }
}

String _str(Object? v, [String fallback = '']) => v is String ? v : fallback;
int? _int(Object? v) => v is num ? v.toInt() : null;
Map<String, dynamic> _map(Object? v) =>
    v is Map<String, dynamic> ? v : const <String, dynamic>{};
List<Map<String, dynamic>> _list(Object? v) =>
    v is List ? v.whereType<Map<String, dynamic>>().toList() : const [];
