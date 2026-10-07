import pytest


@pytest.fixture
def sample_data():
    return [1, 2, 3]


def helper():
    return "help"


@app.route("/index")
def index():
    return helper()


def unused_func():
    pass
