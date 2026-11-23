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

## start on boot (Raspberry Pi)

Turntable is a TUI, so it runs on the Pi's screen from the autologin shell, not
from a systemd service (a service has no terminal to draw on).

1. Log the user in automatically on the console (the user is the one running
   `sudo`):

   ```bash
   sudo raspi-config nonint do_boot_behaviour B2
   ```

   Check with `cat /etc/systemd/system/getty@tty1.service.d/autologin.conf`:
   it should show `--autologin <user>`. Password still applies to SSH and
   `sudo`.

2. Start turntable from `~/.profile` on `tty1` only, so SSH logins do not start
   a second copy. Use `~/.profile`, not `~/.bash_profile`: once the latter
   exists, bash stops reading `~/.profile` and its PATH setup.

   ```bash
   cat >> ~/.profile <<'EOF'

   # start turntable on the Pi's screen after autologin (not over SSH)
   if [ "$(tty)" = "/dev/tty1" ]; then
       deadline=$((SECONDS + 10))
       until turntable run; do
           if [ $SECONDS -ge $deadline ]; then
               echo "turntable still failing after 10s, giving up"
               break
           fi
           echo "turntable failed, retrying in 1s"
           sleep 1
       done
   fi
   EOF
   ```

   If `turntable` is not on PATH at login, use the full path from
   `which turntable`.

Why the retry: for the first few seconds after boot PipeWire's default output
is still a 2-channel device, and the 4-channel interface appears a moment
later. The first run fails with `... offers 2 usable output channels;
routing ... needs 4`; a run a second or two later works. A clean quit
ends the loop and leaves a normal shell.

## useful commands

Create a virtual Pipewire device for testing:

```
pw-loopback -m '[ FL FR SL SR ]' --name=turntable-4ch --capture-props='media.class=Audio/Sink'
```
