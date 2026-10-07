from models.base import BaseModel


class Product(BaseModel):
    def __init__(self, title, price):
        self.title = title
        self.price = price
        self.data = {"title": title, "price": price}

    def display_price(self):
        return f"${self.price:.2f}"
