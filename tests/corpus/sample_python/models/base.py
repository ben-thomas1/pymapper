class BaseModel:
    def validate(self):
        if not self.data:
            return False
        if len(self.data) > 1000:
            return False
        return True

    def to_dict(self):
        return {"data": self.data}

    def save(self):
        if self.validate():
            return True
        return False

    def serialize(self):
        return str(self.to_dict())

    def is_valid(self):
        return self.validate()
