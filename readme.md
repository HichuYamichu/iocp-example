A rough example of how to use Windows IOCP to implement a pipe server. I had a lot of trouble figuring out how to do something real with iocp so hopefully this example helps someone. Keep in mind this is not a reference implementation. I suspect many things could be done in simpler fashion or possibly with less `unsafe`. How this example handles clean up is especially ugly.  
 
