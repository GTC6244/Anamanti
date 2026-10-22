import 'package:flutter/material.dart';

/// Hosts a tab screen the way HomeShell does (inside a Scaffold).
Widget host(Widget child) => MaterialApp(home: Scaffold(body: child));
