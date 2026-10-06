# turntable

## building

Install the required system dependencies:

```bash
sudo apt install -y \
    libpipewire-0.3-dev \
    libclang-dev \
    libudev-dev \
    libasound2-dev
```

`sdl2` for working with touchpad is built with default features. To disable:
```
cargo install --path . --no-default-features
```


## useful commands

Create a virtual Pipewire device for testing:

```
pw-loopback -m '[ FL FR SL SR ]' --name=turntable-4ch --capture-props='media.class=Audio/Sink'
```
