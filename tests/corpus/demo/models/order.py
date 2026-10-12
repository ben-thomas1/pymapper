from models.base import BaseModel
from repo.order_repo import next_order_id  # PM301 cycle + PM303 contract violation


class Order(BaseModel):
    def __init__(self, customer, items):
        self.id = next_order_id()
        self.customer = customer
        self.items = items
        self.data = {"customer": customer, "items": items}

    def total(self):
        return sum(item["price"] * item["qty"] for item in self.items)

    def archive(self):  # PM403: never called
        self.data["archived"] = True
        return self.data
