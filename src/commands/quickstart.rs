//! Quickstart documentation for testrepository

use crate::commands::Command;
use crate::error::Result;
use crate::ui::UI;

pub struct QuickstartCommand;

impl QuickstartCommand {
    pub fn new() -> Self {
        QuickstartCommand
    }
}

impl Command for QuickstartCommand {
    fn execute(&self, ui: &mut dyn UI) -> Result<i32> {
        let help = r#"# Test Repository

## Overview

This project provides a database of test results which can be used as part of
developer workflow to ensure/check things like:

* No commits without having had a test failure, test fixed cycle.
* No commits without new tests being added.
* What tests have failed since the last commit (to run just a subset).
* What tests are currently failing and need work.

Test results are inserted using subunit (and thus anything that can output
subunit or be converted into a subunit stream can be accepted).

## Licensing

Test Repository is under BSD / Apache 2.0 licences.

## Quick Start

Create a config file:

```sh
$ touch .testr.conf
```

Create a repository:

```sh
$ testr init
```

Load a test run into the repository:

```sh
$ testr load < testrun
```

Query the repository:

```sh
$ testr stats
$ testr last
$ testr failing
```

Delete a repository:

```sh
$ rm -rf .testrepository
```

## Documentation

More detailed documentation can be found in the original Python version at
https://testing-cabal.github.io/testrepository/
"#;
        ui.output(help)?;
        Ok(0)
    }

    fn name(&self) -> &str {
        "quickstart"
    }

    fn help(&self) -> &str {
        "Show quickstart documentation for testrepository"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::test_ui::TestUI;

    #[test]
    fn test_quickstart_command() {
        let mut ui = TestUI::new();
        let cmd = QuickstartCommand::new();
        let result = cmd.execute(&mut ui);

        assert_eq!(result.unwrap(), 0);
        assert!(!ui.output.is_empty());
        let output = ui.output.join("\n");
        assert!(output.contains("Quick Start"));
        assert!(output.contains("testr init"));
        assert!(output.contains("testr load"));
    }
}
