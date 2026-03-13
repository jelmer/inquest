# Grouping tests

In certain scenarios you may want to group tests of a certain type together
so that they will be run by the same backend. The `group_regex` option in
your configuration file permits this. When set, tests are grouped by the
group(0) of any regex match. Tests with no match are not grouped.

For example, extending the Python sample configuration with a group regex
that will group test cases together by class (the last `.` splits the class
and test method):

```toml
test_command = "python -m subunit.run discover . $LISTOPT $IDOPTION"
test_id_option = "--load-list $IDFILE"
test_list_option = "--list"
group_regex = '([^\.]+\.)+'
```
