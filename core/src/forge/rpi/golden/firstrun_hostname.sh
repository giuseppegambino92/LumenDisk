#!/bin/sh

set +e

IMAGER_HOSTNAME='h'
CURRENT_HOSTNAME=$(cat /etc/hostname | tr -d " \t\n\r")
if [ -f /usr/lib/raspberrypi-sys-mods/imager_custom ]; then
   /usr/lib/raspberrypi-sys-mods/imager_custom set_hostname "$IMAGER_HOSTNAME"
else
   echo "$IMAGER_HOSTNAME" >/etc/hostname
   IMAGER_HOSTNAME_SED=$(printf '%s' "$IMAGER_HOSTNAME" | sed -e 's/[\\/&]/\\&/g')
   sed -i "s/127.0.1.1.*$CURRENT_HOSTNAME/127.0.1.1\t$IMAGER_HOSTNAME_SED/g" /etc/hosts
fi
FIRSTUSER=$(getent passwd 1000 | cut -d: -f1)
FIRSTUSERHOME=$(getent passwd 1000 | cut -d: -f6)
rm -f /boot/firstrun.sh
sed -i 's| systemd.run.*||g' /boot/cmdline.txt
exit 0
