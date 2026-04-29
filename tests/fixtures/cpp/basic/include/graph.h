#pragma once

#include <string>
#include <vector>

namespace graph {

/// A node in the graph.
class Node {
 public:
  Node(int id, std::string label);
  int id() const;
  const std::string& label() const;

 private:
  int id_;
  std::string label_;
};

/// Generic container parameterized by element type.
template <typename T>
class Container {
 public:
  void add(const T& item);
  size_t size() const;

 private:
  std::vector<T> items_;
};

/// An edge inherits Node and adds endpoint info.
class Edge : public Node {
 public:
  Edge(int id, std::string label, int from, int to);
  int from() const;
  int to() const;

 private:
  int from_;
  int to_;
};

}  // namespace graph
