# GodspeedOS extensive self-check suite.
# Run it with:  gs> selfcheck      (runs this embedded suite IN MEMORY — no disk write,
# so it is not capped by the on-disk file size; it just needs a flashed GSFS drive for
# the file-command tests). Passes iff the summary says "failed 0".
#
# Covers every shell utility's main functions + negative cases, EXCEPT:
#   - observe : it is a live full-screen view (only `observe now` is a snapshot; tested).
#   - drives  : flashing/relabel/reset touch disks and prompt y/N — not scriptable.
# Re-runnable: everything is created under /sc and deleted at the end.
#
# Rules this suite obeys (so it self-grades correctly):
#   - `assert ok|fails|fails-with <cmd>` is the RESULT form — only for NON-piped commands
#     (a line with '|' is a pipeline; the trailing `assert` is its sink instead).
#   - `<producer> | … | assert contains|lacks|empty <text>` is the CONTENT form.
#   - match/count/first/last are byte filters; where/select/sort/to/from work on records.

# ===== meta: the result model + the assert forms themselves =====
assert ok echo hello
assert fails totallybogus
assert fails-with Unknown totallybogus
assert ok result
echo one two three | assert contains two
echo keep this | assert lacks secret
echo nothing | match zzz | assert empty

# ===== self-documentation: <util> help / <util> version =====
assert ok help
assert ok status help
assert ok read help
assert ok assert help
assert ok find version
assert ok clear help

# ===== system info (these print directly; they are not pipe producers) =====
assert ok about
assert ok cores
assert ok mem
assert ok date
assert ok date epoch

# ===== introspection producers: status / caps =====
assert ok status
status | assert contains shell
status | where name=shell | assert contains shell
status | where core=0 | assert contains shell
status | select name core | assert contains shell
status | sort name | assert contains supervisor
assert ok caps
caps shell | assert contains introspect
caps shell | where resource=spawn | assert contains spawn
assert fails caps nosuchservice
assert fails-with FileNotFound caps nosuchservice

# ===== lifecycle guardrails (negative only — safe, deterministic) =====
assert fails spawn supervisor
assert fails-with Denied spawn supervisor
assert fails spawn init
assert fails kill supervisor
assert fails-with Denied kill supervisor
assert fails kill shell
assert fails spawn nosuchservice
assert fails kill nosuchservice
assert fails restart supervisor

# ===== files: create / read / overwrite / append =====
mkdir /sc
assert ok ls /sc
assert fails mkdir /sc
write /sc/a.txt hello
read /sc/a.txt | assert contains hello
write /sc/a.txt world
read /sc/a.txt | assert contains world
read /sc/a.txt | assert lacks hello
write append /sc/a.txt MORE
read /sc/a.txt | assert contains worldMORE
write append /sc/fresh.txt born
read /sc/fresh.txt | assert contains born
assert fails read /sc/missing.txt
assert fails-with FileNotFound read /sc/missing.txt

# ===== directories: mkdir (parents) + delete guard =====
assert fails mkdir /sc/x/y/z
mkdir /sc/x/y/z parents
assert ok ls /sc/x/y/z
mkdir /sc/d1
write /sc/d1/f.txt data
assert fails delete /sc/d1
assert ok read /sc/d1/f.txt

# ===== copy / move / rename (positive + negative) =====
copy /sc/a.txt /sc/b.txt
read /sc/b.txt | assert contains worldMORE
assert fails copy /sc/missing.txt /sc/z.txt
copy /sc/d1 /sc/d2 recursive
assert ok read /sc/d2/f.txt
move /sc/b.txt /sc/c.txt
assert ok read /sc/c.txt
assert fails read /sc/b.txt
assert fails move /sc/missing.txt /sc/q.txt
rename /sc/c.txt renamed.txt
assert ok read /sc/renamed.txt
write /sc/keep.txt x
assert fails rename /sc/renamed.txt keep.txt

# ===== cd: relative paths + negative =====
cd /sc
assert ok read a.txt
ls | assert contains a.txt
cd -
assert ok read /sc/a.txt
assert fails cd /sc/a.txt
cd /

# ===== ls / find / tree as record producers =====
ls /sc | where type=file | assert contains a.txt
ls /sc | where type=dir | assert contains d1
ls /sc | select name | assert contains a.txt
find a.txt /sc | assert contains /sc/a.txt
find f.txt /sc | where type=file | assert contains /sc/d1/f.txt
assert ok find nomatchxyz /sc
tree /sc | assert contains d1
tree /sc | assert contains x

# ===== byte pipes: producers + filters (each line spawns a service; kept lean) =====
greet | assert contains hello
greet | match capability | assert contains capability
greet | count | assert contains 3 lines
greet | sort | first 1 | assert contains capability
greet | sort | last 1 | assert contains ambient
echo lower CASE | upper | assert contains LOWER CASE
echo alpha beta gamma | match beta | assert contains beta

# ===== record service over the binary wire codec (roster) — one line per operator =====
assert ok roster
roster | where role=core | assert contains Matthew
roster | where role!=core | assert lacks Matthew
roster | where core=0 | assert contains Matthew
roster | where core>0 | assert lacks Matthew
roster | where core<2 | assert contains Mark
roster | where name~ar | assert contains Mark
roster | sort reverse core | assert contains John
roster | to json | assert contains role
roster | select name core | to json | assert contains Luke

# ===== json <-> records bridge =====
write /sc/data.json [{"name":"x","n":1},{"name":"y","n":2},{"name":"z","n":3}]
read /sc/data.json | from json | assert contains y
read /sc/data.json | from json | where n>1 | assert contains z
read /sc/data.json | from json | where n>1 | assert lacks x
read /sc/data.json | from json | where n<2 | assert contains x
read /sc/data.json | from json | where n=2 | assert contains y
read /sc/data.json | from json | select name | assert contains z
read /sc/data.json | from json | sort reverse n | assert contains z
read /sc/data.json | from json | to yaml | assert contains name

# ===== cleanup: proves delete + delete recursive =====
delete /sc/a.txt
assert fails read /sc/a.txt
delete /sc recursive
assert fails ls /sc
