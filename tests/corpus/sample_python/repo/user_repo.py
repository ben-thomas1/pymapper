def save_user(user):
    user.validate()
    user.is_valid()      # inherited from BaseModel — will be flagged dead
    data = user.to_dict()
    text = user.serialize()  # inherited from BaseModel — will be flagged dead
    return data


def delete_user(user_id):
    pass
