import 'package:flutter/material.dart';
import 'package:flutter_test/flutter_test.dart';

import 'package:photon_mobileapp/main.dart';

void main() {
  testWidgets('renders health check button', (WidgetTester tester) async {
    await tester.pumpWidget(const PhotonApp());
    expect(find.text('Check server health'), findsOneWidget);
    expect(find.textContaining('Server status:'), findsOneWidget);
  });
}
