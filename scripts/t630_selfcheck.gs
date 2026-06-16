# GodspeedOS T630 self-check suite.
# Bake:  osdev script-disk build/t630.img scripts/t630_selfcheck.gs
# Flash: dd if=build/t630.img of=/dev/sdX bs=4M   (the data drive / a spare USB)
# Run:   gs> run /t630_selfcheck.gs   ->  passes iff the summary says "failed 0".
# Designed for a FRESHLY baked disk (it creates its own files); re-bake to re-run.

# --- the disk: baked GSFS, mounted on boot, suite file present ---
assert ok read /t630_selfcheck.gs
ls / | assert contains t630_selfcheck.gs
drives | assert contains GSFS

# --- file commands: create / read / append / copy / move / rename / find / tree ---
mkdir /t
write /t/a.txt hello
read /t/a.txt | assert contains hello
write append /t/a.txt world
read /t/a.txt | assert contains helloworld
copy /t/a.txt /t/b.txt
read /t/b.txt | assert contains hello
move /t/b.txt /t/c.txt
assert ok read /t/c.txt
assert fails read /t/b.txt
rename /t/c.txt d.txt
assert ok read /t/d.txt
mkdir /t/sub
write /t/sub/deep.txt nested
find deep /t | assert contains /t/sub/deep.txt
tree /t | assert contains sub

# --- record producers + verbs (typed pipes) ---
status | where name=shell | assert contains shell
caps shell | assert contains introspect
caps shell | where resource=spawn | assert contains spawn
ls /t | where type=file | assert contains a.txt
ls /t | where type=dir | assert contains sub
find a.txt /t | where type=file | assert contains /t/a.txt
observe now | sort reverse ticks | assert contains shell

# --- a record SERVICE over the binary wire codec (no from json) ---
roster | where role=core | assert contains vesta
roster | where role=worker | assert lacks vesta
roster | select name core | to json | assert contains hermes

# --- byte pipes: filter built-ins + service filters ---
greet | assert contains hello
greet | match ambient | assert contains ambient
greet | count | assert contains 3 lines
greet | sort | first 1 | assert contains capability
echo lower text | upper | assert contains LOWER TEXT

# --- json <-> records bridge ---
write /j.json [{"name":"x","n":1},{"name":"y","n":2}]
read /j.json | from json | where n>1 | assert contains y
read /j.json | from json | to yaml | assert contains name

# --- the Result model: positive, negative, exact-variant ---
assert ok status
assert ok mem
assert fails read /does/not/exist
assert fails-with FileNotFound read /nope
assert fails spawn supervisor
assert fails-with Denied spawn supervisor
assert fails totallybogus

# --- cleanup proves delete works ---
delete /t/a.txt
assert fails read /t/a.txt
delete /t recursive
assert fails ls /t
delete /j.json
