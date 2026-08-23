# turntable

## useful commands

Create a virtual Pipewire device for testing:

```
pw-loopback -m '[ FL FR SL SR ]' --name=turntable-4ch --capture-props='media.class=Audio/Sink'
```
