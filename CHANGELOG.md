# Changelog

The format is based on [Keep a Changelog].

[Keep a Changelog]: http://keepachangelog.com/en/1.0.0/

## [v0.4.1] - 2023-04-21

This ia a small-patch release that enables the feature flag `doc_cfg` to 
render feature-gated APIs properly on the documentation website.

## [v0.4.0] - 2023-04-20

A new release that makes the library support WASM and two feature flags, `native` and `web` are introduced
to select whether to compile the library for `native` or `web` but the default is still `native`.

Some other fixes were:
- Re-export all types in the public API
- Support other retry strategies for calls and subscriptions.
- Improve the reconnect API to know when the reconnection started and was completed.
- Provide an error type.
- Improve internal feature flag handling.

Thanks to [@seunlanlege](https://github.com/seunlanlege) who did the majority of the work
to get the library working for WASM.

## [v0.3.0] - 2023-02-08

### Changed
- chore(deps): update jsonrpsee from 0.21 to 0.22 ([#20](https://github.com/niklasad1/reconnecting-jsonrpsee-ws-client/pull/20))

## [v0.2.0] - 2023-01-24

This release reworks the APIs a little bit and the major changes are:
- The subscription emits an error `DisconnectWillReconnect` once
a reconnection attempt occurs. It also contains the reason why it
was closed.
- Add API to subscribe to reconnections.
- Expose low-level APIs for subscriptions and requests.
- Upgrade jsonrpsee to v0.21.
- Rename `Client::retry_count` to `Client::reconnect_count`.
- Minor documentation tweaks.
- Modify the WebSocket ping/pong API.

## [v0.1.0] - 2022-01-04

Initial release of the crate.
