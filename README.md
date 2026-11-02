# turntable

Runs on Linux only. It plays through PipeWire when PipeWire is running and
through ALSA otherwise; recording needs PipeWire.

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

## music and recorder drives

FAT32 (`vfat`) drives must be mounted with the `utf8` option. Without it the
kernel mounts them with `iocharset=ascii` and shows every non-ASCII letter as
`?`, so `Köppchen.mp3` appears as `K?ppchen.mp3` and cards for such tracks fail
with `No such file or directory`. exFAT drives take `iocharset=utf8` instead.

Example `/etc/fstab` lines (find the UUID with `lsblk -f`):

```
UUID=BACB-C356 /mnt/music     vfat  defaults,ro,nofail,utf8 0 0
UUID=1212-001C /mnt/recorder  vfat  defaults,nofail,uid=1000,gid=1000,umask=022,utf8 0 0
```

Apply after editing (stop turntable first):

```bash
sudo cp /etc/fstab /etc/fstab.bak
sudo systemctl daemon-reload
sudo umount /mnt/music /mnt/recorder
sudo mount -a
mount | grep /mnt/   # each line should include utf8
```

`-o remount` may not apply the new charset; unmount and mount again.
`umount` failing with "target is busy": `sudo fuser -vm /mnt/recorder` shows
what holds the drive.

## useful commands

Create a virtual Pipewire device for testing:

```
pw-loopback -m '[ FL FR SL SR ]' --name=turntable-4ch --capture-props='media.class=Audio/Sink'
```
