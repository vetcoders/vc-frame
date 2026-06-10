# The following snippet is meant to be used like this in your fish config:
#
# if status is-interactive
#     # Configure auto-attach/exit to your likings (default is off).
#     # set VC_FRAME_AUTO_ATTACH true
#     # set VC_FRAME_AUTO_EXIT true
#     eval (vc-frame setup --generate-auto-start fish | string collect)
# end
if not set -q VC_FRAME; and not set -q ZELLIJ
    if test "$VC_FRAME_AUTO_ATTACH" = "true"; or test "$ZELLIJ_AUTO_ATTACH" = "true"
        vc-frame attach -c
    else
        vc-frame
    end

    if test "$VC_FRAME_AUTO_EXIT" = "true"; or test "$ZELLIJ_AUTO_EXIT" = "true"
        kill $fish_pid
    end
end
