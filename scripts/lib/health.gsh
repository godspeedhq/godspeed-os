# health - a one-shot GodspeedOS health dashboard (system library).
#
# Composes the system-info utilities into one view, so you get the whole picture
# without typing six commands. It is a gsh SCRIPT, not a Rust built-in: baked into
# the image and resolved PATH-like (just type `health`). The first citizen of the
# system library - proof that features grow by userspace composition, not new kernel
# or service surface. Add a script to scripts/lib/ and it becomes a command like this.

echo '== GodspeedOS health =='
date
cores
uptime
mem
net
drives
echo '== end of health =='
