# turntable

## building

`sdl2` is built from C source and is only needed for the touchpad input, so a
machine without a C toolchain builds without it:

```
cargo build --no-default-features
```

Such a build drives the decks from the MIDI controller only; `-I touchpad` is
not offered, and `-I auto` fails when no controller is connected.

## useful commands

Create a virtual Pipewire device for testing:

```
pw-loopback -m '[ FL FR SL SR ]' --name=turntable-4ch --capture-props='media.class=Audio/Sink'
```
