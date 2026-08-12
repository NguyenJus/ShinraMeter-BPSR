# shinra-bpsr

Minimal damage tracker for Blue Protocol: Star Resonance, styled after
ShinraMeter. Tracks damage per player, resets cleanly, and nothing else.

- Runs on Windows (packet capture of game traffic; approved by the game's
  developers).
- Developed from WSL; cross-compiled to `x86_64-pc-windows-gnu`.

## Build

```sh
cargo check --target x86_64-pc-windows-gnu   # type-check from WSL (no linker needed)
cargo build --release --target x86_64-pc-windows-gnu  # needs mingw-w64
```

## References

Packet-format knowledge derives from these open-source trackers:
- https://github.com/Blue-Protocol-Source/BPSR-ZDPS
- https://github.com/winjwinj/bpsr-logs
- https://github.com/resonance-logs/resonance-logs
- UI inspiration: https://github.com/neowutran/ShinraMeter
