#!/bin/sh
# Джарвис на колонке: запуск, остановка, удаление. Всё лежит в /data/opt/jarvisd,
# системные файлы не меняются (профиль запуска Алисы подкладывается bind-mount'ом).
#   jarvisd.sh boot       — из /etc/init.d/S98jarvisd при загрузке (до старта Алисы)
#   jarvisd.sh start|stop — запустить/остановить сам jarvisd
#   jarvisd.sh status
#   jarvisd.sh uninstall  — убрать всё и перезагрузить колонку
D=/data/opt/jarvisd
RUN=/tmp/jarvisd
LAUNCH=/system/vendor/quasar/launch.json
PID=$RUN/jarvisd.pid
LOOP=$RUN/loop.pid
LOG=$RUN/jarvisd.log
INIT=/etc/init.d/S98jarvisd

mounted() { grep -q " $1 " /proc/mounts; }
running() { [ -f "$1" ] && kill -0 "$(cat "$1")" 2>/dev/null; }

mic_on() {
    # профиль запуска Алисы с нашим vqe_tap.so; если не получилось — Джарвис работает без микрофона
    [ -f "$D/no-mic" ] && return 0
    mounted "$LAUNCH" && return 0
    core="$D/jarvisd"
    [ -x "$core" ] || core="$D/jarvisd.next-rust"
    if "$core" patch-launch "$LAUNCH" "$RUN/launch.json" "$D/vqe_tap.so" "$RUN/mic.sock" 2>>"$LOG"; then
        mount --bind "$RUN/launch.json" "$LAUNCH"
    fi
}

loop() {
    exec "$D/jarvis-supervisor" "$D" "$RUN" >>"$RUN/supervisor.log" 2>&1
}

start() {
    mkdir -p "$RUN"
    running "$LOOP" && return 0
    [ -x "$D/jarvis-supervisor" ] || return 1
    # отдельный сеанс: не умирает вместе с adb/ssh, из которых запущен
    start-stop-daemon -S -b -m -p "$LOOP" -x /bin/sh -- "$D/jarvisd.sh" loop
}

stop() {
    running "$LOOP" && kill "$(cat "$LOOP")" 2>/dev/null
    running "$PID" && kill "$(cat "$PID")" 2>/dev/null
    rm -f "$LOOP" "$PID"
}

mic_state() {
    mounted "$LAUNCH" || { echo "хук не настроен"; return; }
    state="не загружен (нужна перезагрузка)"
    for p in /proc/[0-9]*; do
        [ -r "$p/cmdline" ] || continue
        if tr '\0' ' ' <"$p/cmdline" 2>/dev/null | grep -q -- '--platformd'; then
            if grep -q 'vqe_tap\.so (deleted)' "$p/maps" 2>/dev/null; then
                echo "устарел (нужна перезагрузка)"
                return
            fi
            if grep -q 'vqe_tap\.so' "$p/maps" 2>/dev/null; then
                if [ -s "$RUN/mic.sock.seen" ]; then
                    state="подключён"
                else
                    state="хук загружен, но звук не поступает"
                fi
            fi
        fi
    done
    echo "$state"
}

case "$1" in
boot)
    [ -f "$D/disabled" ] && exit 0
    mkdir -p "$RUN"
    mic_on
    start
    ;;
start) start ;;
loop) loop ;;
stop) stop ;;
restart) stop; sleep 1; start ;;
status)
    echo "jarvisd: $(running "$PID" && echo работает || echo остановлен) ($("$D/jarvisd" version 2>/dev/null))"
    [ -s "$RUN/port" ] && echo "панель: порт $(cat "$RUN/port" 2>/dev/null)"
    echo "микрофон: $(mic_state)"
    echo "автозапуск: $([ -f "$D/disabled" ] && echo выключен || echo включён)"
    ;;
uninstall)
    stop
    touch "$D/disabled"
    while mounted "$LAUNCH"; do umount "$LAUNCH" || break; done
    mount -o remount,rw / 2>/dev/null
    rm -f "$INIT"
    mount -o remount,ro / 2>/dev/null
    rm -rf "$D" "$RUN"
    sync
    reboot
    ;;
*) echo "usage: $0 boot|start|stop|restart|status|uninstall"; exit 2 ;;
esac
