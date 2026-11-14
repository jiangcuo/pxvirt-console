# Network (F2)

[Documentation](README.md) · [中文](../zh/network.md)

| Page | Shows | Actions |
|---|---|---|
| 1 Management | Management bridge, address, gateway, DNS, link state, reachability | Change IP / gateway `e`, bridged NIC `b`, DNS `d`, ping gateway `P`, apply `A`, discard `V`; rescue: reset management network without the API `R`, stop the PVE firewall `F` |
| 2 Interfaces | Bridges, bonds, VLANs and NICs (PVE network config) with pending changes, link speed, MAC | Create bridge / bond / VLAN `n` (`b` `o` `v`), edit `e`, delete `x`, apply `A` (ifreload), revert `V` |
| 3 Traffic | Live rates of every interface with history, packets, errors, drops | – |
| 4 Connections | TCP / UDP sockets: protocol, state, local and peer address, owning process, receive / send rate, RTT, total traffic, retransmissions | Only this process `p`, disconnect `x` (`ss -K`), terminate process `K` (SIGTERM, type its name) |
| 5 Tools | ping, traceroute, DNS lookup, port check | – |
| 6 Routes | Routing tables (IPv4 / IPv6) | – |
| 7 Firewall | pve-firewall state and rules | Start `s`, emergency stop `S` |

## Management network safety

Changes to the management network are applied with an automatic rollback: if you do not confirm within 60 s (for
example because the new address is unreachable), the previous configuration is restored by a systemd timer.

## Connections

- `f` switches established / listening / all, `o` sorts by traffic / process / peer.
- Rates come from the kernel byte counters of each socket (`ss -i`), measured between refreshes (2 s).
- Disconnecting uses `ss -K`; the kernel needs `CONFIG_INET_DIAG_DESTROY` (otherwise `ss` reports an error).
- Needs host access (local or SSH mode).
