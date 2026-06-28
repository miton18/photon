# Photon — Mobile app (Flutter)

This is a manual scaffold: `pubspec.yaml`, `lib/main.dart`, `test/`. The native
platform folders (`android/`, `ios/`, etc.) are **not** generated yet because the
Flutter SDK is not installed on this machine.

## Bootstrap

```bash
# Install the Flutter SDK first: https://docs.flutter.dev/get-started/install
flutter create .   # fills in android/ ios/ macos/ web/ windows/ linux/
flutter pub get
flutter run
```

`flutter create .` is non-destructive: it keeps the existing `lib/main.dart` and
`pubspec.yaml` and only adds the missing platform scaffolding.
