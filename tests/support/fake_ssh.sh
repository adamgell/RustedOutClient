#!/bin/sh

socket=
operation=
master=false
proxy=false
previous=

for argument in "$@"; do
    if [ "$previous" = "-S" ]; then
        socket=$argument
    elif [ "$previous" = "-O" ]; then
        operation=$argument
    fi
    if [ "$argument" = "-M" ]; then
        master=true
    fi
    case "$argument" in
        "exec /usr/sbin/qm vncproxy "*) proxy=true ;;
    esac
    previous=$argument
done

if [ -z "$socket" ]; then
    exit 70
fi

if [ -f "${socket}.track_all_pids" ]; then
    printf '%s\n' "$$" >> "${socket}.children"
fi

if [ "$proxy" != true ] && [ "${LC_PVE_TICKET+x}" = x ]; then
    exit 73
fi

{
    printf '%s\n' '<invoke>'
    printf '%s\n' "$@"
} >> "${socket}.argv"

state="${socket}.state"
pid_file="${socket}.pid"

if [ "$master" = true ]; then
    printf '%s\n' "$$" > "$pid_file"
    : > "$state"
    trap 'rm -f "$state" "$pid_file"; exit 0' TERM INT HUP
    while [ -f "$state" ]; do
        sleep 0.05
    done
    rm -f "$pid_file"
    exit 0
fi

if [ "$operation" = "check" ]; then
    printf '%s\n' "$$" > "${socket}.check.pid"
    if [ -f "${socket}.hang_check" ]; then
        while :; do
            sleep 0.05
        done
    fi
    if [ -f "$state" ]; then
        exit 0
    fi
    printf '%s\n' 'ssh: control master is not running' >&2
    exit 1
fi

if [ "$operation" = "exit" ]; then
    printf '%s\n' "$$" > "${socket}.exit.pid"
    if [ -f "${socket}.hang_exit" ]; then
        while :; do
            sleep 0.05
        done
    fi
    if [ -f "${socket}.ignore_exit" ]; then
        exit 0
    fi
    if [ -f "$pid_file" ]; then
        pid=$(sed -n '1p' "$pid_file")
        kill -TERM "$pid" 2>/dev/null || true
    fi
    exit 0
fi

if [ "$proxy" = true ]; then
    case "${LC_PVE_TICKET-}" in
        ????????) ;;
        *) exit 71 ;;
    esac
    case "$LC_PVE_TICKET" in
        *[!A-Za-z0-9]*) exit 72 ;;
    esac
    printf '%s\n' "$$" > "${socket}.proxy.pid"
    if [ -f "${socket}.proxy_track_opens" ]; then
        printf '%s\n' "$$" >> "${socket}.proxy.opens"
    fi
    : > "${socket}.proxy.env-valid"
    if [ -f "${socket}.proxy_rfb_security_failure" ]; then
        printf 'RFB 003.008\n'
        dd bs=1 count=12 of=/dev/null 2>/dev/null || exit 74
        printf '\001\001'
        : > "${socket}.proxy.negotiation-failure-sent"
        /bin/cat >/dev/null
        exit 0
    fi
    if [ -f "${socket}.proxy_malformed_first_frame" ]; then
        printf 'RFB 003.008\n'
        dd bs=1 count=12 of=/dev/null 2>/dev/null || exit 74
        printf '\001\002'
        dd bs=1 count=1 of=/dev/null 2>/dev/null || exit 74
        printf '0123456789abcdef'
        dd bs=1 count=16 of=/dev/null 2>/dev/null || exit 74
        printf '\000\000\000\000'
        dd bs=1 count=1 of=/dev/null 2>/dev/null || exit 74
        printf '\000\001\000\001'
        printf '\040\030\000\001\000\377\000\377\000\377\020\010\000\000\000\000'
        printf '\000\000\000\000'
        dd bs=1 count=62 of=/dev/null 2>/dev/null || exit 74
        printf '\000\000\000\001'
        printf '\000\000\000\000\000\002\000\001\000\000\000\000'
        : > "${socket}.proxy.first-frame-sent"
        /bin/cat >/dev/null
        exit 0
    fi
    if [ -f "${socket}.proxy_rfb_input_capture" ]; then
        if (set -C; : > "${socket}.proxy.input-capture-claimed") 2>/dev/null; then
            printf 'RFB 003.008\n'
            dd bs=1 count=12 of=/dev/null 2>/dev/null || exit 74
            printf '\001\002'
            dd bs=1 count=1 of=/dev/null 2>/dev/null || exit 74
            printf '0123456789abcdef'
            dd bs=1 count=16 of=/dev/null 2>/dev/null || exit 74
            printf '\000\000\000\000'
            dd bs=1 count=1 of=/dev/null 2>/dev/null || exit 74
            printf '\000\001\000\001'
            printf '\040\030\000\001\000\377\000\377\000\377\020\010\000\000\000\000'
            printf '\000\000\000\000'
            dd bs=1 count=62 of=/dev/null 2>/dev/null || exit 74
            printf '\000\000\000\001'
            printf '\000\000\000\000\000\001\000\001\000\000\000\000'
            printf '\000\000\000\000'
            dd bs=1 count=10 of=/dev/null 2>/dev/null || exit 74
            dd bs=1 count=8 of="${socket}.proxy.input-wire" 2>/dev/null || exit 74
            : > "${socket}.proxy.key-down-read"
            dd bs=1 count=8 >> "${socket}.proxy.input-wire" 2>/dev/null || exit 74
            : > "${socket}.proxy.key-up-read"
            if [ -f "${socket}.proxy_input_capture_hang_after_keyup" ]; then
                while :; do
                    sleep 0.05
                done
            fi
            /bin/cat >/dev/null
            exit 0
        fi
        : > "${socket}.proxy.input-capture-skipped"
    fi
    if [ ! -f "${socket}.proxy_no_stdout" ]; then
        printf 'RFB 003.008\n'
    fi
    if [ -f "${socket}.proxy_stderr_open" ]; then
        (
            trap 'exit 0' PIPE TERM INT HUP
            while printf 'x' >&2; do
                sleep 0.05
            done
        ) &
        printf '%s\n' "$!" > "${socket}.proxy.stderr-holder.pid"
    fi
    if [ -f "${socket}.proxy_eof_live" ]; then
        exec 1>&-
        while :; do
            sleep 0.05
        done
    fi
    if [ -f "${socket}.proxy_large_stderr" ]; then
        awk 'BEGIN { for (i = 0; i < 100; i++) printf "%01024d", 0 }' >&2
    fi
    dd bs=1 count=1 of=/dev/null 2>/dev/null || true
    if [ -f "${socket}.proxy_wait_for_eof" ]; then
        /bin/cat >/dev/null
        exit 0
    fi
    if [ -f "${socket}.proxy_auth_failure" ]; then
        printf '%s\n' 'synthetic@pve.example.invalid: Permission denied (publickey).' >&2
        exit 255
    fi
    if [ -f "${socket}.hang_proxy" ]; then
        if [ -f "${socket}.proxy_live_probe" ]; then
            while :; do
                printf '%s' x >> "${socket}.proxy.live-heartbeat"
                sleep 0.05
            done
        else
            while :; do
                sleep 0.05
            done
        fi
    fi
    exit 0
fi

if [ -f "${socket}.hold_before_inventory_ready" ]; then
    : > "${socket}.inventory.spawned"
    while [ -f "${socket}.hold_before_inventory_ready" ] &&
        [ ! -f "${socket}.allow_inventory_ready" ]; do
        sleep 0.05
    done
fi

printf '%s\n' "$$" > "${socket}.inventory.pid"

if [ -f "${socket}.hang_inventory" ]; then
    while :; do
        sleep 0.05
    done
fi

if [ -f "${socket}.inventory_payload" ]; then
    exec /bin/cat "${socket}.inventory_payload"
fi

if [ -f "${socket}.large_stderr" ]; then
    : > "${socket}.inventory_running"
    awk 'BEGIN { for (i = 0; i < 100; i++) printf "%01024d", 0 }' >&2
    rm -f "${socket}.inventory_running"
fi

if [ -f "${socket}.malformed" ]; then
    printf '%s\n' '[{"vmid":107,"name":"bad","node":"pve2","status":"paused"}]'
    exit 0
fi

if [ -f "${socket}.matching_node" ]; then
    printf '%s\n' '[{"vmid":107,"name":"LABZ1-CM01","node":"pve2","status":"running","template":0}]'
    exit 0
fi

if [ -f "${socket}.mismatched_node" ]; then
    printf '%s\n' '[{"vmid":107,"name":"LABZ1-CM01","node":"pve1","status":"running","template":0}]'
    exit 0
fi

printf '%s\n' '[{"vmid":205,"name":"LabZ1-APP01","status":"stopped","template":0},{"vmid":301,"name":"template-base","status":"stopped","template":1},{"vmid":107,"name":"LABZ1-CM01","status":"running","template":0}]'
