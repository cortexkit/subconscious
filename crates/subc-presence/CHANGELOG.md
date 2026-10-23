# Changelog

## 0.1.3

- Test only: the Windows pre-check test still requires the first check to return within 5 s, but asks for a real answer from the operating system only after any slow first start of Windows Hello's COM (Component Object Model) server has finished, since that start can outlast the 4 s wait on a busy machine.

## 0.1.2

- Treat a locked or non-interactive Mac as having no one present, and retain the LocalAuthentication error domain and code so the daemon can log why the prompt failed.

## 0.1.1

- Use the sample prompt “approve a request from module: write (requested by a local program)” in the unsupported-platform unit test so its input matches the sentence format constructed by subc-daemon.
