# Changelog

## 0.2.1

- Report an authorized non-interactive polkit preflight as `Outcome::AuthorizedWithoutPrompt`, distinct from an interactive approval, so callers can require a person to confirm a request.

## 0.2.0

- Add one-shot Linux polkit authentication with literal per-request text, per-user policy rendering, and same-connection withdrawal. Distinguish missing setup and lack of a usable authentication challenge from a person's decline.
- Make `Outcome` non-exhaustive and add setup/provider-error reasons. This is a breaking release rather than a 0.1 patch so older daemons with exhaustive matches cannot resolve an incompatible enum through their existing caret requirements. Callers must refuse unknown outcomes.
- Refuse effective-root Linux daemons before contacting polkit, which otherwise authorizes root subjects without authentication. Bound withdrawal, including each cancellation call, to five seconds while retaining the prompt's withdrawn state until its eventual reply.

## 0.1.3

- Test only: the Windows pre-check test still requires the first check to return within 5 s, but asks for a real answer from the operating system only after any slow first start of Windows Hello's COM (Component Object Model) server has finished, since that start can outlast the 4 s wait on a busy machine.

## 0.1.2

- Treat a locked or non-interactive Mac as having no one present, and retain the LocalAuthentication error domain and code so the daemon can log why the prompt failed.

## 0.1.1

- Use the sample prompt “approve a request from module: write (requested by a local program)” in the unsupported-platform unit test so its input matches the sentence format constructed by subc-daemon.
