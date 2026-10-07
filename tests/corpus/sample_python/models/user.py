from models.base import BaseModel


class User(BaseModel):
    def __init__(self, name, email):
        self.name = name
        self.email = email
        self.data = {"name": name, "email": email}

    def display_name(self):
        return self.name.title()
