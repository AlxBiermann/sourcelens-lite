class OrderRepository:
    def __init__(self, db):
        self.db = db

    def get_order(self, order_id):
        return self.db.find("orders", order_id)


def total_price(items):
    return sum(item.price * item.quantity for item in items)
