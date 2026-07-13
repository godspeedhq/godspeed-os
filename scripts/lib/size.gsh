# size - total bytes of the files under a directory tree (system library).
#
# Answers "how big is this tree?" - the purpose the POSIX fossil `du` never said out
# loud. One record pipe: `find * <path>` walks the tree (glob * matches every entry,
# each file row carrying its byte size), `where type=file` keeps the files, `sum size`
# reduces to the total. Pure composition of the record model - no new walking code
# (26.2; docs/records.md). Bare `size` sums the whole disk from /.
if $argcount == 0 {
    find * | where type=file | sum size
} else if $arg1 == version {
    echo 'size 0.1.0'
    echo 'Copyright (C) 2026 Bankole Ogundero and the GodspeedOS contributors.'
} else if $arg1 == help {
    echo 'size 0.1.0 - total bytes of the files under a tree'
    echo ''
    echo 'usage:'
    echo '  size [path]                 sum every file under the tree (default /)'
    echo '      e.g. size /docs'
    echo '  size version                print the version'
    echo '  size help                   print this message'
} else {
    find * $arg1 | where type=file | sum size
}
