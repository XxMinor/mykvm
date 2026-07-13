# Balanced Input and Sync Reliability Design

## Goal

Keep input latency low while preserving reliable clipboard and file transfer behavior under disconnects, congestion, and multiple peers.

## Architecture

The QUIC transport will schedule real-time datagrams independently from reliable streams. Datagram health and cooldown state will be tracked per peer so one unreachable device cannot poison another. Mouse movement may be coalesced or dropped under pressure, while keyboard, button, clipboard, and file data retain explicit success/failure semantics.

Reliable streams will use bounded priority classes. Clipboard/control traffic takes priority over bulk file chunks. File transfer receive handling will acknowledge safe duplicate chunks so an ACK loss does not abort an otherwise valid transfer.

## Behavior

- Input datagrams remain non-blocking and have a bounded pending budget.
- Each peer has independent consecutive-failure and cooldown state.
- Successful traffic clears only that peer's failure state.
- Stream sends run concurrently with datagram scheduling instead of blocking the transport command loop.
- Reliable stream concurrency is bounded to control memory and connection pressure.
- Duplicate file chunks already written at the expected previous index are accepted without writing twice.
- Queue pressure and peer failures remain visible through logs and focused unit tests.

## Compatibility

No wire protocol version change is required. Existing peers continue to decode the same packets. Duplicate-chunk acceptance only relaxes receiver behavior and remains compatible with older senders.

## Validation

- Unit tests cover peer-isolated health, bounded stream scheduling, and duplicate file chunks.
- Existing Rust tests must remain green.
- Frontend lint and production build must pass.
- No changes include the untracked `claude_auto_continue.py` file.
