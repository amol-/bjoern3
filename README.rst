bjoern3: Fast And Ultra-Lightweight HTTP/1.1 WSGI Server
========================================================

A screamingly fast, ultra-lightweight WSGI_ server for CPython 3.8+,
written in C using Marc Lehmann's high performance libev_ event loop and
Ryan Dahl's http-parser_.

Why It's Cool
~~~~~~~~~~~~~
bjoern3 is the *fastest*, *smallest* and *most lightweight* WSGI server out there,
featuring

* ~ 1000 lines of C code
* Memory footprint ~ 600KB
* Python 3.8+ support
* Single-threaded and without coroutines or other crap
* Can bind to TCP `host:port` addresses and Unix sockets (thanks @k3d3!)
* Full persistent connection ("*keep-alive*") support in both HTTP/1.0 and 1.1,
  including support for HTTP/1.1 chunked responses

Installation
~~~~~~~~~~~~
``pip install bjoern3``.

Usage
~~~~~

Flask example
-------------

.. code-block:: python

   from flask import Flask

   app = Flask(__name__)

   @app.route("/")
   def hello_world():
       return "Hello, World!"

   if __name__ == "__main__":
       import bjoern

       bjoern.run(app, "127.0.0.1", 8000)


Advanced usage
--------------

.. code-block:: python

   # Bind to TCP host/port pair:
   bjoern.run(wsgi_application, host, port)

   # TCP host/port pair, enabling SO_REUSEPORT if available.
   bjoern.run(wsgi_application, host, port, reuse_port=True)

   # Bind to Unix socket:
   bjoern.run(wsgi_application, 'unix:/path/to/socket')

   # Bind to abstract Unix socket: (Linux only)
   bjoern.run(wsgi_application, 'unix:@socket_name')


Alternatively, the mainloop can be run separately:

.. code-block:: python

   bjoern.listen(wsgi_application, host, port)
   bjoern.run()


You can also simply pass a Python socket(-like) object. Note that you are responsible
for initializing and cleaning up the socket in that case.

.. code-block:: python

   bjoern.server_run(socket_object, wsgi_application)
   bjoern.server_run(filedescriptor_as_integer, wsgi_application)

The Python module name remains ``bjoern``, even though the distribution name is ``bjoern3``.


.. _WSGI:         http://www.python.org/dev/peps/pep-0333/
.. _libev:        http://software.schmorp.de/pkg/libev.html
.. _http-parser:  https://github.com/joyent/http-parser
