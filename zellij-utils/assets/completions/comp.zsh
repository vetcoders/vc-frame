function zr () { vc-frame run --name "$*" -- zsh -ic "$*";}
function zrf () { vc-frame run --name "$*" --floating -- zsh -ic "$*";}
function zri () { vc-frame run --name "$*" --in-place -- zsh -ic "$*";}
function ze () { vc-frame edit "$*";}
function zef () { vc-frame edit --floating "$*";}
function zei () { vc-frame edit --in-place "$*";}
function zpipe () { 
  if [ -z "$1" ]; then
    vc-frame pipe;
  else 
    vc-frame pipe -p $1;
  fi
}
