import 'dart:convert';

import 'package:flutter/material.dart';
import 'package:http/http.dart' as http;

void main() => runApp(const PhotonApp());

class PhotonApp extends StatelessWidget {
  const PhotonApp({super.key});

  @override
  Widget build(BuildContext context) {
    return MaterialApp(
      title: 'Photon',
      theme: ThemeData(colorSchemeSeed: Colors.indigo, useMaterial3: true),
      home: const HomePage(),
    );
  }
}

class HomePage extends StatefulWidget {
  const HomePage({super.key});

  @override
  State<HomePage> createState() => _HomePageState();
}

class _HomePageState extends State<HomePage> {
  // Use 10.0.2.2 on the Android emulator to reach the host machine.
  static const _serverUrl = String.fromEnvironment(
    'SERVER_URL',
    defaultValue: 'http://10.0.2.2:3000',
  );

  String _status = 'unknown';

  Future<void> _checkHealth() async {
    try {
      final res = await http.get(Uri.parse('$_serverUrl/api/health'));
      final body = jsonDecode(res.body) as Map<String, dynamic>;
      setState(() => _status = body['status'] as String? ?? 'no status');
    } catch (e) {
      setState(() => _status = 'error: $e');
    }
  }

  @override
  Widget build(BuildContext context) {
    return Scaffold(
      appBar: AppBar(title: const Text('Photon')),
      body: Center(
        child: Column(
          mainAxisAlignment: MainAxisAlignment.center,
          children: [
            Text('Server status: $_status'),
            const SizedBox(height: 16),
            FilledButton(
              onPressed: _checkHealth,
              child: const Text('Check server health'),
            ),
          ],
        ),
      ),
    );
  }
}
