# Voice

Tebis can handle Telegram voice notes in two directions:

- voice input: you send a voice note, Tebis transcribes it locally, then sends
  the text to your agent
- voice replies: Tebis reads agent replies back as Telegram voice messages

Both features are optional.

## Voice input

Voice input uses Whisper through `whisper.cpp`. The model is downloaded once
and cached in the Tebis data directory.

Enable it during setup, or set:

```sh
TELEGRAM_STT=on
TELEGRAM_STT_MODEL=small.en
```

Useful settings:

| Variable | Default | Meaning |
| --- | --- | --- |
| `TELEGRAM_STT_MODEL` | `small.en` | Whisper model key |
| `TELEGRAM_STT_LANGUAGE` | `en` | Spoken language |
| `TELEGRAM_STT_MAX_DURATION_SEC` | `120` | Maximum voice note length |

## Voice replies

Voice replies are text-to-speech messages sent back to Telegram.

| Backend | Platform | Notes |
| --- | --- | --- |
| `say` | macOS | Built in |
| `winrt` | Windows | Built in, uses installed Windows voices |
| `kokoro-local` | macOS, Linux, Windows | Offline neural voice, needs local dependencies |
| `kokoro-remote` | Any | OpenAI-compatible speech endpoint |

By default, Tebis sends a voice reply only after you sent a voice message. Set
this if you want every text reply to also come back as voice:

```sh
TELEGRAM_TTS_RESPOND_TO_ALL=on
```

## macOS built-in voice

```sh
TELEGRAM_TTS_BACKEND=say
TELEGRAM_TTS_VOICE=Samantha
```

## Windows built-in voice

```sh
TELEGRAM_TTS_BACKEND=winrt
TELEGRAM_TTS_VOICE=Zira
```

Leave `TELEGRAM_TTS_VOICE` empty to use the system default voice.

## Local Kokoro voice

Local Kokoro runs offline. Build Tebis with the feature enabled:

```sh
cargo build --release --features kokoro-local
```

Then configure:

```sh
TELEGRAM_TTS_BACKEND=kokoro-local
TELEGRAM_TTS_MODEL=kokoro-v1.0
TELEGRAM_TTS_VOICE=af_sarah
```

Local Kokoro may need `espeak-ng` and ONNX Runtime. The setup wizard tries to
guide you through the platform-specific requirements.

## Remote Kokoro or another speech server

Use an OpenAI-compatible speech endpoint:

```sh
TELEGRAM_TTS_BACKEND=kokoro-remote
TELEGRAM_TTS_REMOTE_URL=https://your-speech-server.example
TELEGRAM_TTS_REMOTE_MODEL=kokoro
TELEGRAM_TTS_VOICE=af_sarah
```

If the server needs a key:

```sh
TELEGRAM_TTS_REMOTE_API_KEY=replace-me
```

Use HTTP only on a trusted local network:

```sh
TELEGRAM_TTS_REMOTE_ALLOW_HTTP=on
```

## Troubleshooting

| Symptom | What to check |
| --- | --- |
| First voice action is slow | Models download once and are cached |
| Voice notes are ignored | Check `TELEGRAM_STT=on` |
| Replies stay text-only | Check `TELEGRAM_TTS_BACKEND` and restart Tebis |
| Every reply should be voice | Set `TELEGRAM_TTS_RESPOND_TO_ALL=on` |
