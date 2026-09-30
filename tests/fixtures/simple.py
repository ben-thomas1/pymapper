def greet(name):
    return f"Hello, {name}"


class Greeter:
    def say_hello(self):
        return greet("world")


def main():
    g = Greeter()
    g.say_hello()
