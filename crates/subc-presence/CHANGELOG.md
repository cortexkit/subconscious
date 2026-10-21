# Changelog

## 0.1.2

- Treat a locked or non-interactive Mac as having no one present, and retain the LocalAuthentication error domain and code so the daemon can log why the prompt failed.

## 0.1.1

- Use the sample prompt “approve a request from module: write (requested by a local program)” in the unsupported-platform unit test so its input matches the sentence format constructed by subc-daemon.
