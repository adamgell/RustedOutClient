#!/bin/sh

socket=
operation=
master=false
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
    previous=$argument
done

if [ -z "$socket" ]; then
    exit 70
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
