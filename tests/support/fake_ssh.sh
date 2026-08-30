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

if [ -f "${socket}.hang_inventory" ]; then
    while :; do
        sleep 0.05
    done
fi

if [ -f "${socket}.exact_limit" ] || [ -f "${socket}.over_limit" ]; then
    output_size=4194304
    if [ -f "${socket}.over_limit" ]; then
        output_size=4194305
    fi
    exec awk -v target="$output_size" 'BEGIN {
        prefix="[{\"vmid\":107,\"name\":\"LABZ1-CM01\",\"status\":\"running\",\"template\":0,\"padding\":\"";
        suffix="\"}]";
        remaining=target-length(prefix)-length(suffix);
        block=sprintf("%01024d", 0);
        printf "%s", prefix;
        while (remaining >= 1024) { printf "%s", block; remaining-=1024; }
        while (remaining > 0) { printf "x"; remaining--; }
        printf "%s", suffix;
    }'
fi

if [ -f "${socket}.large_stderr" ]; then
    : > "${socket}.inventory_running"
    awk 'BEGIN { for (i = 0; i < 100; i++) printf "%01024d", 0 }' >&2
    rm -f "${socket}.inventory_running"
fi

if [ -f "${socket}.oversized" ]; then
    exec awk 'BEGIN { for (i = 0; i < 4195; i++) printf "%01024d", 0 }'
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
