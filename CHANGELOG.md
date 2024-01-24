# Changelog

The format is based on [Keep a Changelog].

[Keep a Changelog]: http://keepachangelog.com/en/1.0.0/

## [v0.2.0] - 2023-01-24

This release reworks the APIs a little bit and the major changes are:
- The subscription emits an error `DisconnectWillReconnect` once
a reconnection attempt occurs. It also contains the reason why it
was closed.
- Add API to subscribe to reconnections.
- Expose low-level APIs for subscriptions and requests.
- Upgrade jsonrpsee to v0.21.
- Rename `Client::retry_count` to `Client::reconnect_count`
- Minor documentation tweaks.

## [v0.1.0] - 2022-01-04

Initial release of the crate.
