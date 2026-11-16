#!/bin/sh

set +e

IMAGER_HOSTNAME='pi-lab'
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
if [ -f /usr/lib/raspberrypi-sys-mods/imager_custom ]; then
   /usr/lib/raspberrypi-sys-mods/imager_custom enable_ssh
else
   systemctl enable ssh
fi
IMAGER_USER='mario'
IMAGER_PASS='$6$LumenDiskTest$golden'
IMAGER_USER_SED=$(printf '%s' "$IMAGER_USER" | sed -e 's/[\\/&]/\\&/g')
if [ -f /usr/lib/userconf-pi/userconf ]; then
   /usr/lib/userconf-pi/userconf "$IMAGER_USER" '$6$LumenDiskTest$golden'
else
   echo "$FIRSTUSER:$IMAGER_PASS" | chpasswd -e
   if [ "$FIRSTUSER" != "$IMAGER_USER" ]; then
      usermod -l "$IMAGER_USER" "$FIRSTUSER"
      usermod -m -d "/home/$IMAGER_USER" "$IMAGER_USER"
      groupmod -n "$IMAGER_USER" "$FIRSTUSER"
      if grep -q "^autologin-user=" /etc/lightdm/lightdm.conf ; then
         sed /etc/lightdm/lightdm.conf -i -e "s/^autologin-user=.*/autologin-user=$IMAGER_USER_SED/"
      fi
      if [ -f /etc/systemd/system/getty@tty1.service.d/autologin.conf ]; then
         sed /etc/systemd/system/getty@tty1.service.d/autologin.conf -i -e "s/$FIRSTUSER/$IMAGER_USER_SED/"
      fi
      if [ -f /etc/sudoers.d/010_pi-nopasswd ]; then
         sed -i "s/^$FIRSTUSER /$IMAGER_USER_SED /" /etc/sudoers.d/010_pi-nopasswd
      fi
   fi
fi
if [ -f /usr/lib/raspberrypi-sys-mods/imager_custom ] && true; then
   /usr/lib/raspberrypi-sys-mods/imager_custom set_wlan 'Lab' '0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef' 'GB'
else
cat >/etc/wpa_supplicant/wpa_supplicant.conf <<'WPAEOF'
country=GB
ctrl_interface=DIR=/var/run/wpa_supplicant GROUP=netdev
ap_scan=1

update_config=1
network={
	ssid="Lab"
	key_mgmt=WPA-PSK SAE
	psk=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
	ieee80211w=1
}
WPAEOF
   chmod 600 /etc/wpa_supplicant/wpa_supplicant.conf
   rfkill unblock wifi
   for filename in /var/lib/systemd/rfkill/*:wlan ; do
       echo 0 > $filename
   done
fi
if [ -f /usr/lib/raspberrypi-sys-mods/imager_custom ]; then
   /usr/lib/raspberrypi-sys-mods/imager_custom set_keymap 'it'
   /usr/lib/raspberrypi-sys-mods/imager_custom set_timezone 'Europe/Rome'
else
   rm -f /etc/localtime
   echo 'Europe/Rome' >/etc/timezone
   dpkg-reconfigure -f noninteractive tzdata
cat >/etc/default/keyboard <<'KBEOF'
XKBMODEL="pc105"
XKBLAYOUT="it"
XKBVARIANT=""
XKBOPTIONS=""

KBEOF
   dpkg-reconfigure -f noninteractive keyboard-configuration
fi
IMAGER_LOCALE='it_IT.UTF-8'
if command -v raspi-config >/dev/null 2>&1; then
   raspi-config nonint do_change_locale "$IMAGER_LOCALE"
else
   sed -i "s/^# *\($IMAGER_LOCALE\)/\1/" /etc/locale.gen
   locale-gen
   update-locale LANG="$IMAGER_LOCALE"
fi
rm -f /boot/userconf.txt /boot/firmware/userconf.txt
rm -f /boot/firstrun.sh
sed -i 's| systemd.run.*||g' /boot/cmdline.txt
exit 0
