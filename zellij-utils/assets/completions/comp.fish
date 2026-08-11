function __fish_complete_sessions
    vc-frame list-sessions --short --no-formatting 2>/dev/null
end
complete -c vc-frame -n "__fish_seen_subcommand_from attach" -f -a "(__fish_complete_sessions)" -d "Session"
complete -c vc-frame -n "__fish_seen_subcommand_from a" -f -a "(__fish_complete_sessions)" -d "Session"
complete -c vc-frame -n "__fish_seen_subcommand_from kill-session" -f -a "(__fish_complete_sessions)" -d "Session"
complete -c vc-frame -n "__fish_seen_subcommand_from k" -f -a "(__fish_complete_sessions)" -d "Session"
complete -c vc-frame -n "__fish_seen_subcommand_from setup" -l "generate-completion" -x -a "bash elvish fish zsh powershell" -d "Shell"
function zr
  command vc-frame run --name "$argv" -- fish -c "$argv"
end
function zrf
  command vc-frame run --name "$argv" --floating -- fish -c "$argv"
end
function zri
  command vc-frame run --name "$argv" --in-place -- fish -c "$argv"
end
function ze
  command vc-frame edit $argv
end
function zef
  command vc-frame edit --floating $argv
end
function zei
  command vc-frame edit --in-place $argv
end

# the zpipe alias and its completions
function __fish_complete_aliases
  vc-frame list-aliases 2>/dev/null
end
function zpipe
  if count $argv > /dev/null
    command vc-frame pipe -p $argv
  else
    command vc-frame pipe
  end
end
complete -c zpipe -f -a "(__fish_complete_aliases)" -d "Zpipes"
