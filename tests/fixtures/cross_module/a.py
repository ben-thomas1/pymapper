from b import foo
from b import bar as b_bar
import utils
from os.path import join

def caller():
    foo()
    b_bar()
    utils.helper()
    join("a", "b")

def local_func():
    pass

def same_name_test():
    local_func()
