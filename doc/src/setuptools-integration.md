# Setuptools integration

inquest provides a setuptools commands for ease of integration with
setuptools-based workflows:

* inq:
  `python setup.py inq` will run inq in parallel mode
  Options that would normally be passed to inq run can be added to the
  inq-options argument.
  `python setup.py inq --inq-options="--failing"` will append `--failing`
  to the test run.
* inq --coverage:
  `python setup.py inq --coverage` will run inq in code coverage mode. This
  assumes the installation of the python coverage module.
* `python inq --coverage --omit=ModuleThatSucks.py` will append
  --omit=ModuleThatSucks.py to the coverage report command.
