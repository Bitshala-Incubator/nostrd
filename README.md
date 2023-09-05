# Nostrd

Introducing a new type of Nostr relay designed for professional use, enabling control, and profit generation.

The relay will consist of a robust backend equipped with an efficient database and configurable NIPs (Nostr Improvement Proposals). Relay operators can easily toggle the NIP service on and off.

This relay will employ gRPC communication to connect with a front-end application, facilitating control, monitoring, and fine-tuning of relay operations.

Furthermore, the relay will establish a connection with a backend Bitcoin node and maintain its wallet. It can be customized to provide additional NIP services, allowing users to earn satoshis and compensate for the relay's running costs.

## Project Status

The project is currently in the pre-alpha stage and is actively under development.

## Roadmap
- [x] Implementation of basic mandatory NIPs.
- [ ] Integration of optional NIPs.
- [ ] Development of a gRPC communication server in the backend.
- [ ] Implementation of unit tests for individual modules.
- [ ] Creation of functional tests for relay operations.
- [ ] Introduction of a simplified web frontend for controlling and monitoring the backend via gRPC.
- [ ] Implementation of a more efficient database interface like Postgres.
- [ ] Integration of a Bitcoin wallet using BDK (Bitcoin Development Kit).
- [ ] Capability to post client advertisements and relay them to all connected clients for satoshi (sats) payment.
- [ ] Development of a simplified example frontend for the paid Nostr marketplace.

