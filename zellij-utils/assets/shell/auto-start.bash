if [[ -z "${VC_FRAME:-$ZELLIJ}" ]]; then
    if [[ "${VC_FRAME_AUTO_ATTACH:-$ZELLIJ_AUTO_ATTACH}" == "true" ]]; then
        vc-frame attach -c
    else
        vc-frame
    fi

    if [[ "${VC_FRAME_AUTO_EXIT:-$ZELLIJ_AUTO_EXIT}" == "true" ]]; then
        exit
    fi
fi
