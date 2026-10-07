class Animal:
    def speak(self):
        return "..."

    def move(self):
        return "moving"


class Dog(Animal):
    def speak(self):
        return "woof"

    def fetch(self):
        self.move()
        return "fetching"


class Puppy(Dog):
    def speak(self):
        return "yip"

    def play(self):
        self.speak()
        self.fetch()
