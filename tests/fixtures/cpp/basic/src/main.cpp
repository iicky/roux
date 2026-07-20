#include <iostream>

#include "graph.h"

int main() {
  graph::Node n(1, "root");
  graph::Edge e(2, "edge", 1, 2);
  std::cout << n.label() << " -> " << e.label() << std::endl;
  return 0;
}
