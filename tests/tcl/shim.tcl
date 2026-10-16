# Harness for SQLite's own TCL test suite (test/*.test in the SQLite
# source tree): defines the `sqlite3` command on top of a database server
# process (examples/tcl_db_server.rs — rustqlite or the bundled SQLite),
# stubs the testfixture's C-only commands, sources SQLite's real
# tester.tcl, and reports each test as `@@PASS name` / `@@FAIL name`.
#
#   tclsh shim.tcl SERVER_BINARY BACKEND SQLITE_TEST_DIR TEST_FILE
#
# Run every file under both backends: a test that passes on `sqlite` and
# fails on `ours` is an engine divergence (tests/tcl/run.py does this).

set ::shim_server  [lindex $argv 0]
set ::shim_backend [lindex $argv 1]
set testdir        [lindex $argv 2]
set ::shim_file    [lindex $argv 3]
set argv {}
set argc 0

set ::srv [open "|[list $::shim_server $::shim_backend]" r+]
fconfigure $::srv -translation binary -buffering full

# One request / reply exchange (see tcl_db_server.rs for the protocol).
proc srv {cmd h fields} {
  set out "$cmd $h [llength $fields]\n"
  foreach f $fields {
    set b [encoding convertto utf-8 $f]
    append out "[string length $b]\n$b"
  }
  puts -nonewline $::srv $out
  flush $::srv
  set head [gets $::srv]
  set n [lindex $head 1]
  set res {}
  for {set i 0} {$i < $n} {incr i} {
    set len [gets $::srv]
    lappend res [read $::srv $len]
  }
  if {[lindex $head 0] ne "OK"} {
    error [encoding convertfrom utf-8 [lindex $res 0]]
  }
  return $res
}

# Run SQL on handle $h with TCL variables from the caller of the db
# command (level $lvl); returns {columns values}.
proc shim_eval {h sql lvl} {
  set fields [list $sql]
  foreach name [srv PARAMS $h [list $sql]] {
    set name [encoding convertfrom utf-8 $name]
    set var [string trimleft $name @]
    upvar [expr {$lvl + 1}] $var v
    if {![info exists v]} {
      lappend fields $name n ""
    } elseif {[string index $name 0] eq "@"} {
      lappend fields $name b $v
    } elseif {[string is wideinteger -strict $v]} {
      lappend fields $name i $v
    } elseif {[string is double -strict $v] && [regexp {[.eEn]} $v]} {
      lappend fields $name r $v
    } else {
      lappend fields $name t $v
    }
  }
  set res [srv EVAL $h $fields]
  set ncol [lindex $res 0]
  set cols {}
  foreach c [lrange $res 1 $ncol] {
    lappend cols [encoding convertfrom utf-8 $c]
  }
  set vals {}
  foreach {t p} [lrange $res [expr {$ncol + 1}] end] {
    switch -- $t {
      i { lappend vals $p }
      r { lappend vals [expr {double($p)}] }
      t { lappend vals [encoding convertfrom utf-8 $p] }
      b { lappend vals $p }
      default { lappend vals $::shim_null($h) }
    }
  }
  return [list $cols $vals]
}

proc sqlite3 {args} {
  if {[lindex $args 0] eq "-has-codec"} { return 0 }
  if {[lindex $args 0] eq "-version"} { return 3.53.4 }
  if {[lindex $args 0] eq "-sourceid"} { return "" }
  set name [lindex $args 0]
  set file [lindex $args 1]
  # Options (-vfs, -readonly, ...) are accepted and ignored.
  catch {srv CLOSE $name {}}
  # The server process keeps the directory it started in; tester.tcl
  # cds into ./testdir — resolve relative names against the TCL side's
  # current directory so file operations (forcedelete, db_save, ...) and
  # the database agree on what "test.db" is.
  if {$file ne "" && $file ne ":memory:" && ![string match "file:*" $file]} {
    set file [file normalize $file]
  }
  srv OPEN $name [list $file]
  set ::shim_null($name) ""
  proc ::$name {method args} [format {
    return [uplevel 1 [list shim_db %s $method] $args]
  } [list $name]]
  return ""
}

proc shim_db {h method args} {
  switch -- $method {
    eval {
      set sql [lindex $args 0]
      lassign [shim_eval $h $sql 1] cols vals
      if {[llength $args] == 1} { return $vals }
      if {[llength $args] == 2} {
        set script [lindex $args 1]
        set arr ""
      } else {
        set arr [lindex $args 1]
        set script [lindex $args 2]
      }
      set n [llength $cols]
      if {$n == 0} { return "" }
      for {set i 0} {$i < [llength $vals]} {incr i $n} {
        set row [lrange $vals $i [expr {$i + $n - 1}]]
        if {$arr eq ""} {
          foreach c $cols v $row { uplevel 1 [list set $c $v] }
        } else {
          uplevel 1 [list array unset $arr]
          uplevel 1 [list set ${arr}(*) $cols]
          foreach c $cols v $row { uplevel 1 [list set ${arr}($c) $v] }
        }
        set code [catch {uplevel 1 $script} r]
        if {$code == 3} break
        if {$code == 4} continue
        if {$code != 0} { return -code $code $r }
      }
      return ""
    }
    one - onecolumn {
      lassign [shim_eval $h [lindex $args 0] 1] cols vals
      return [lindex $vals 0]
    }
    exists {
      lassign [shim_eval $h [lindex $args 0] 1] cols vals
      return [expr {[llength $vals] > 0}]
    }
    close {
      srv CLOSE $h {}
      rename ::$h {}
      return ""
    }
    changes { return [lindex [srv CHANGES $h {}] 0] }
    total_changes { return [lindex [srv TOTAL $h {}] 0] }
    last_insert_rowid { return [lindex [srv ROWID $h {}] 0] }
    nullvalue {
      if {[llength $args]} { set ::shim_null($h) [lindex $args 0] }
      return $::shim_null($h)
    }
    transaction {
      set type ""
      if {[llength $args] == 2} { set type [lindex $args 0] }
      set script [lindex $args end]
      set auto [lindex [srv AUTOCOMMIT $h {}] 0]
      if {$auto} { shim_eval $h "BEGIN $type" 0 }
      set code [catch {uplevel 1 $script} r]
      if {$auto} {
        if {$code == 0 || $code == 2 || $code == 3 || $code == 4} {
          shim_eval $h COMMIT 0
        } else {
          catch {shim_eval $h ROLLBACK 0}
        }
      }
      return -code $code $r
    }
    cache - busy - timeout - config - status - enable_load_extension - progress -
    trace - trace_v2 - profile - interrupt - wal_hook - update_hook -
    commit_hook - rollback_hook - preupdate - errorcode - limit {
      return ""
    }
    default {
      error "shim: unsupported db method $method"
    }
  }
}

# Every compile-time capability on except the testfixture internals.
foreach o {
  8_3_names allow_rowid_in_view altertable analyze api_armor atomicwrite attach
  auth autoinc autoindex autoreset autovacuum bloblit builtin_test cast check
  columnmetadata compileoption_diags complete compound conflict cte curdir
  datetime deprecated deserialize explain floatingpoint foreignkey fts5
  gettable hiddencolumns incrblob integrityck json1 like_match_blobs like_opt
  mathlib memorydb mergesort or_opt ordered_set_aggregates pager_pragmas pragma
  progress reindex rtree schema_pragmas schema_version subquery tempdb trigger
  update_delete_limit vacuum view vtab wal windowfunc normalize offset_sql_func
  null_trim stmtvtab carray stat4 utf16 trace preupdate session
} { set ::sqlite_options($o) 1 }
foreach o {
  crashtest debug default_autovacuum default_ckptfullfsync direct_read dirsync
  fast_secure_delete fts3 fts3_unicode fts4_deferred has_codec icu icu_collations
  legacyformat lfs load_ext lock_proxy_pragmas lookaside malloc_usable_size mem5
  memorymanage mmap multiplex_ext_overwrite mutex oversize_cell_check
  prefer_proxy_locking rbu rowid32 scanstatus secure_delete setlk_timeout
  shared_cache shared_chache snapshot sqllog tclvar thread_misuse_warnings
  threadsafe threadsafe1 threadsafe2 unlock_notify uri_00_error win32malloc wsd
  cursorhints break
  # Small knobs the corpus probes that were missing: mutex_noop (the
  # shim presents a real-mutex build - run_thread_tests, called from
  # EVERY finish_test, errors out without it), memdebug (no
  # SQLITE_MEMDEBUG), casesensitivelike (default off; the pragma
  # toggles it at runtime), configslower, yytrackmaxstackdepth.
  mutex_noop memdebug casesensitivelike configslower yytrackmaxstackdepth
} { set ::sqlite_options($o) 0 }
set ::sqlite_options(default_autovacuum) 0

# test1.c's fpnum_compare: token-wise equality, floating-point tokens
# compared to 14 significant digits.
proc fpnum_compare {a b} {
  if {[catch {llength $a} na] || [catch {llength $b} nb]} {
    return [string equal $a $b]
  }
  if {$na != $nb} { return 0 }
  foreach x $a y $b {
    if {$x eq $y} continue
    if {[string is double -strict $x] && [string is double -strict $y]
        && [format %.14g $x] eq [format %.14g $y]} continue
    return 0
  }
  return 1
}

# testfixture commands implemented in C: unknown ones answer "".
proc unknown {cmd args} {
  if {![info exists ::shim_unknown($cmd)]} {
    set ::shim_unknown($cmd) 1
    puts stderr "@@UNKNOWN $cmd"
  }
  return ""
}

set ::G(start:permutation) ""
set ::argv0 [file join $testdir tester.tcl]
source [file join $testdir tester.tcl]
set ::cmdlinearg(maxerror) 1000000

rename do_test shim_do_test_orig
proc do_test {name cmd expected} {
  set before [set_test_counter errors]
  set code [catch {uplevel 1 [list shim_do_test_orig $name $cmd $expected]} r]
  if {[set_test_counter errors] > $before} {
    puts_original "@@FAIL $name"
  } else {
    puts_original "@@PASS $name"
  }
  return -code $code $r
}

# Test files find tester.tcl through `[file dirname $argv0]`.
set ::argv0 $::shim_file
source $::shim_file
